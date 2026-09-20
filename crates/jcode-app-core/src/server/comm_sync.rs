use super::services::SwarmServiceHandle;
use super::ClientConnectionInfo;
use crate::agent::Agent;
use crate::protocol::{
    AgentStatusSnapshot, PlanGraphStatus, ServerEvent, SessionActivitySnapshot,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};

type SessionAgents = Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>;

pub(super) struct CommResyncPlanContext<'a> {
    pub(super) client_event_tx: &'a mpsc::UnboundedSender<ServerEvent>,
    pub(super) swarm: &'a SwarmServiceHandle,
}

fn live_activity_snapshot(
    connections: &HashMap<String, ClientConnectionInfo>,
    session_id: &str,
    fallback_processing: bool,
) -> Option<SessionActivitySnapshot> {
    let mut processing_without_tool = false;
    let mut tool_name = None;
    for info in connections.values() {
        if info.session_id != session_id || !info.is_processing {
            continue;
        }
        if let Some(current_tool_name) = info.current_tool_name.clone() {
            tool_name = Some(current_tool_name);
            break;
        }
        processing_without_tool = true;
    }

    tool_name
        .map(|current_tool_name| SessionActivitySnapshot {
            is_processing: true,
            current_tool_name: Some(current_tool_name),
        })
        .or_else(|| {
            processing_without_tool.then_some(SessionActivitySnapshot {
                is_processing: true,
                current_tool_name: None,
            })
        })
        .or_else(|| {
            fallback_processing.then_some(SessionActivitySnapshot {
                is_processing: true,
                current_tool_name: None,
            })
        })
}

/// Recent-token lookback window used when reporting per-agent churn in
/// `swarm list`. Short enough to reflect "what is this agent doing right now".
pub(super) const SWARM_LIST_TOKEN_WINDOW_SECS: u64 = 10;

/// Runtime extras for a swarm member, gathered without holding the agent lock
/// for long. Used to enrich the `swarm list` roster with live activity,
/// provider/model, token churn, turn count, and todo progress.
#[derive(Default)]
pub(super) struct MemberRuntimeExtras {
    pub(super) activity: Option<SessionActivitySnapshot>,
    pub(super) provider_name: Option<String>,
    pub(super) provider_model: Option<String>,
    pub(super) provider_effort: Option<String>,
    pub(super) turn_count: Option<u64>,
    pub(super) recent_total_tokens: Option<u64>,
    pub(super) recent_output_tokens: Option<u64>,
    pub(super) recent_window_secs: Option<u64>,
    pub(super) cumulative_total_tokens: Option<u64>,
    pub(super) last_activity_age_secs: Option<u64>,
    pub(super) todos_completed: Option<usize>,
    pub(super) todos_total: Option<usize>,
}

/// Gather live runtime extras for a single member session.
///
/// `member_is_running` is used as a fallback "processing" hint when no live
/// client connection is reporting activity (e.g. headless sessions).
pub(super) async fn member_runtime_extras(
    session_id: &str,
    member_is_running: bool,
    sessions: &SessionAgents,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
) -> MemberRuntimeExtras {
    let activity = {
        let connections = client_connections.read().await;
        live_activity_snapshot(&connections, session_id, member_is_running)
    };

    let (provider_name, provider_model, provider_effort) = {
        let agent_sessions = sessions.read().await;
        if let Some(agent) = agent_sessions.get(session_id) {
            // Never block on a busy agent: token churn and turns come from the
            // lock-free metrics registry, so a missing provider name here just
            // means the agent is mid-turn.
            if let Ok(agent) = agent.try_lock() {
                (
                    Some(agent.provider_name()),
                    Some(agent.provider_model()),
                    agent.provider_reasoning_effort(),
                )
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        }
    };

    let metrics = crate::session_metrics::snapshot(
        session_id,
        std::time::Duration::from_secs(SWARM_LIST_TOKEN_WINDOW_SECS),
    );

    let (todos_completed, todos_total) = match crate::todo::load_todos(session_id) {
        Ok(todos) if !todos.is_empty() => {
            let completed = todos.iter().filter(|t| t.status == "completed").count();
            (Some(completed), Some(todos.len()))
        }
        _ => (None, None),
    };

    MemberRuntimeExtras {
        activity,
        provider_name,
        provider_model,
        provider_effort,
        turn_count: metrics.map(|m| m.turns),
        recent_total_tokens: metrics.map(|m| m.recent_total_tokens),
        recent_output_tokens: metrics.map(|m| m.recent_output_tokens),
        recent_window_secs: metrics.map(|_| SWARM_LIST_TOKEN_WINDOW_SECS),
        cumulative_total_tokens: metrics.map(|m| m.cumulative_total_tokens),
        last_activity_age_secs: metrics.and_then(|m| m.last_activity_age_secs),
        todos_completed,
        todos_total,
    }
}

pub(super) async fn handle_comm_summary(
    id: u64,
    req_session_id: String,
    target_session: String,
    limit: Option<usize>,
    sessions: &SessionAgents,
    swarm: &SwarmServiceHandle,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    if !swarm
        .ensure_same_swarm_access(id, &req_session_id, &target_session, client_event_tx)
        .await
    {
        return;
    }

    let limit = limit.unwrap_or(10);
    let agent_sessions = sessions.read().await;
    if let Some(agent) = agent_sessions.get(&target_session) {
        let tool_calls = if let Ok(agent) = agent.try_lock() {
            agent.get_tool_call_summaries(limit)
        } else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!(
                    "Session '{}' is busy; try summary again shortly",
                    target_session
                ),
                retry_after_secs: Some(1),
            });
            return;
        };
        let _ = client_event_tx.send(ServerEvent::CommSummaryResponse {
            id,
            session_id: target_session,
            tool_calls,
        });
    } else {
        let _ = client_event_tx.send(ServerEvent::CommSummaryResponse {
            id,
            session_id: target_session,
            tool_calls: Vec::new(),
        });
    }
}

pub(super) async fn handle_comm_status(
    id: u64,
    req_session_id: String,
    target_session: String,
    sessions: &SessionAgents,
    swarm: &SwarmServiceHandle,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let file_touch = swarm.file_touch();
    if !swarm
        .ensure_same_swarm_access(id, &req_session_id, &target_session, client_event_tx)
        .await
    {
        return;
    }

    let swarm_members = &swarm.swarm_state().members;
    let snapshot = {
        let members = swarm_members.read().await;
        let Some(member) = members.get(&target_session) else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!("Unknown session '{target_session}'"),
                retry_after_secs: None,
            });
            return;
        };

        let files_touched = file_touch
            .sorted_file_strings_for_session(&target_session)
            .await;

        let activity = {
            let connections = client_connections.read().await;
            live_activity_snapshot(&connections, &target_session, member.status == "running")
        };

        let (provider_name, provider_model) = {
            let agent_sessions = sessions.read().await;
            if let Some(agent) = agent_sessions.get(&target_session) {
                if let Ok(agent) = agent.try_lock() {
                    (Some(agent.provider_name()), Some(agent.provider_model()))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        };

        AgentStatusSnapshot {
            session_id: member.session_id.clone(),
            friendly_name: member.friendly_name.clone(),
            swarm_id: member.swarm_id.clone(),
            status: Some(member.status.clone()),
            detail: member.detail.clone(),
            role: Some(member.role.clone()),
            is_headless: Some(member.is_headless),
            live_attachments: Some(member.event_txs.len()),
            status_age_secs: Some(member.last_status_change.elapsed().as_secs()),
            last_activity_age_secs: crate::session_metrics::last_activity_age_secs(&target_session),
            joined_age_secs: Some(member.joined_at.elapsed().as_secs()),
            files_touched,
            activity,
            provider_name,
            provider_model,
        }
    };

    let _ = client_event_tx.send(ServerEvent::CommStatusResponse { id, snapshot });
}

pub(super) async fn handle_comm_read_context(
    id: u64,
    req_session_id: String,
    target_session: String,
    sessions: &SessionAgents,
    swarm: &SwarmServiceHandle,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    if !swarm
        .ensure_same_swarm_access(id, &req_session_id, &target_session, client_event_tx)
        .await
    {
        return;
    }

    if !swarm
        .can_read_full_context(&req_session_id, &target_session)
        .await
    {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Only the coordinator, worktree manager, or the target session may read full context. Use summary for lightweight access.".to_string(),
            retry_after_secs: None,
        });
        return;
    }

    let agent_sessions = sessions.read().await;
    if let Some(agent) = agent_sessions.get(&target_session) {
        let messages = if let Ok(agent) = agent.try_lock() {
            agent.get_history()
        } else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!(
                    "Session '{}' is busy; try read_context again shortly",
                    target_session
                ),
                retry_after_secs: Some(1),
            });
            return;
        };
        let _ = client_event_tx.send(ServerEvent::CommContextHistory {
            id,
            session_id: target_session,
            messages,
        });
    } else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: format!("Unknown session '{target_session}'"),
            retry_after_secs: None,
        });
    }
}

pub(super) async fn handle_comm_plan_status(
    id: u64,
    req_session_id: String,
    swarm: &SwarmServiceHandle,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let swarm_members = &swarm.swarm_state().members;
    let swarm_plans = &swarm.swarm_state().plans;
    let swarm_id = {
        let members = swarm_members.read().await;
        members
            .get(&req_session_id)
            .and_then(|member| member.swarm_id.clone())
    };

    let Some(swarm_id) = swarm_id else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
        return;
    };

    let summary = {
        let plans = swarm_plans.read().await;
        let plan = plans.get(&swarm_id);
        if let Some(plan) = plan {
            PlanGraphStatus::from_versioned_plan(swarm_id.clone(), plan, Some(8), Vec::new())
        } else {
            PlanGraphStatus::empty_for_swarm(swarm_id.clone())
        }
    };

    let _ = client_event_tx.send(ServerEvent::CommPlanStatusResponse { id, summary });
}

pub(super) async fn handle_comm_resync_plan(
    id: u64,
    req_session_id: String,
    ctx: &CommResyncPlanContext<'_>,
) {
    let client_event_tx = ctx.client_event_tx;
    let swarm = ctx.swarm;
    swarm
        .resync_plan_participant(id, &req_session_id, client_event_tx)
        .await;
}
