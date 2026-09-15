//! Server-initiated ("wake") turns for live sessions.
//!
//! Several server paths start a full conversation turn in a session without
//! that session's client sending a message: swarm DM/broadcast wake delivery,
//! background-task completion wakes, scheduled-task delivery, and post-reload
//! resume. Those turns must keep the same bookkeeping as client-initiated
//! turns, otherwise the swarm member status stays "ready/idle" while the agent
//! is actually streaming and attached TUIs never learn the turn finished.
//!
//! This module is the single shared implementation: it marks the member
//! `running` while the turn streams, flips it back to `ready` (with a
//! completion report) or `failed` at the end, and fans out a terminal
//! `Done`/`Error` event (id 0) so attached clients can settle the externally
//! started turn in their UI.

use super::client_lifecycle::process_locked_message_streaming_mpsc;
use super::services::SwarmServiceHandle;
use super::{SwarmMember, session_event_fanout_sender, truncate_detail};
use crate::agent::Agent;
use crate::protocol::ServerEvent;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

type SessionAgents = Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>;

/// Reserve the live agent for `session_id` when the session has at least one
/// live client attachment and its agent is currently idle.
///
/// The returned guard *is* the reservation: it stays held until the tracked
/// turn finishes, so two concurrent wakes cannot both observe the agent as
/// idle and then serialize behind each other (#1152).
pub(super) async fn idle_live_agent(
    session_id: &str,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<OwnedMutexGuard<Agent>> {
    let agent = {
        let guard = sessions.read().await;
        guard.get(session_id).cloned()
    }?;

    let has_live_attachments = {
        let members = swarm_members.read().await;
        members
            .get(session_id)
            .map(|member| !member.event_txs.is_empty() || !member.event_tx.is_closed())
            .unwrap_or(false)
    };
    if !has_live_attachments {
        return None;
    }

    agent.try_lock_owned().ok()
}

/// Spawn `message` as a full tracked turn in a live session.
///
/// Mirrors the client-initiated turn lifecycle: the swarm member is marked
/// `running` before the turn starts and `ready` (with a completion report) or
/// `failed` when it finishes. A synthetic terminal `Done { id: 0 }` (or
/// `Error { id: 0, .. }`) is fanned out to attached clients so their UI can
/// finish rendering the externally started turn.
pub(super) async fn spawn_tracked_live_turn(
    session_id: &str,
    mut agent: OwnedMutexGuard<Agent>,
    message: String,
    system_reminder: Option<String>,
    display_role: Option<crate::session::StoredDisplayRole>,
    status_detail: Option<String>,
    swarm: SwarmServiceHandle,
) {
    swarm
        .set_member_status(session_id, "running", status_detail)
        .await;

    let event_tx = session_event_fanout_sender(
        session_id.to_string(),
        Arc::clone(&swarm.swarm_state.members),
    );
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        let start_message_index = agent.message_count();
        let result = if let Some(display_role) = display_role {
            agent
                .run_once_streaming_mpsc_with_display_role(
                    &message,
                    vec![],
                    system_reminder,
                    event_tx.clone(),
                    Some(display_role),
                )
                .await
        } else {
            process_locked_message_streaming_mpsc(
                &mut agent,
                &message,
                vec![],
                system_reminder,
                event_tx.clone(),
            )
            .await
        };
        let completion_report = result
            .is_ok()
            .then(|| agent.latest_assistant_text_after(start_message_index))
            .flatten();
        // Keep the reservation until after the terminal status is published.
        // Releasing it earlier lets a follow-up wake reserve the agent and
        // publish `running`, which this turn's later `ready`/`failed` would
        // then overwrite, hiding the newer turn and suppressing its
        // coordinator completion notification.
        let reservation = agent;
        match result {
            Ok(()) => {
                swarm
                    .set_member_status_with_report(&session_id, "ready", None, completion_report)
                    .await;
                let _ = event_tx.send(ServerEvent::Done { id: 0 });
            }
            Err(error) => {
                crate::logging::error(&format!(
                    "Server-initiated turn failed for live session {}: {}",
                    session_id, error
                ));
                swarm
                    .set_member_status(
                        &session_id,
                        "failed",
                        Some(truncate_detail(&error.to_string(), 120)),
                    )
                    .await;
                let _ = event_tx.send(ServerEvent::Error {
                    id: 0,
                    message: crate::util::format_error_chain(&error),
                    retry_after_secs: None,
                });
            }
        }
        drop(reservation);
    });
}

/// Run `message` immediately as a tracked turn if the session is live and
/// idle. Returns `true` when the turn was started.
pub(super) async fn run_live_turn_if_idle(
    session_id: &str,
    message: &str,
    system_reminder: Option<String>,
    sessions: &SessionAgents,
    swarm: &SwarmServiceHandle,
) -> bool {
    let Some(agent) = idle_live_agent(session_id, sessions, &swarm.swarm_state.members).await else {
        return false;
    };
    let detail = Some(truncate_detail(message, 120)).filter(|detail| !detail.is_empty());
    spawn_tracked_live_turn(
        session_id,
        agent,
        message.to_string(),
        system_reminder,
        None,
        detail,
        swarm.clone(),
    )
    .await;
    true
}

pub(super) async fn run_live_system_turn_if_idle(
    session_id: &str,
    message: &str,
    sessions: &SessionAgents,
    swarm: &SwarmServiceHandle,
) -> bool {
    let Some(agent) = idle_live_agent(session_id, sessions, &swarm.swarm_state.members).await else {
        return false;
    };
    let detail = Some(truncate_detail(message, 120)).filter(|detail| !detail.is_empty());
    spawn_tracked_live_turn(
        session_id,
        agent,
        message.to_string(),
        None,
        Some(crate::session::StoredDisplayRole::System),
        detail,
        swarm.clone(),
    )
    .await;
    true
}
