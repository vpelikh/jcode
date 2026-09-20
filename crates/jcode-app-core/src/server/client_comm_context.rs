use super::debug::ClientConnectionInfo;
use super::services::SwarmServiceHandle;
use super::{
    SwarmEventType, SwarmMember, fanout_session_event, record_swarm_event,
};
use crate::protocol::{AgentInfo, ContextEntry, NotificationType, ServerEvent};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};

async fn swarm_id_for_session(
    session_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<String> {
    let members = swarm_members.read().await;
    members.get(session_id).and_then(|m| m.swarm_id.clone())
}

async fn friendly_name_for_session(
    session_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<String> {
    let members = swarm_members.read().await;
    members
        .get(session_id)
        .and_then(|member| member.friendly_name.clone())
}

pub(super) async fn handle_comm_share(
    id: u64,
    req_session_id: String,
    key: String,
    value: String,
    append: bool,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    // Swarm-domain state is reached through the swarm service handle. These
    // locals keep the body single-homed on the handle's fields (server service
    // split, convergence slice).
    let swarm_members = &swarm.swarm_state.members;
    let swarms_by_id = &swarm.swarm_state.swarms_by_id;
    let (event_history, event_counter, swarm_event_tx) = swarm.read_event_sources();
    let swarm_id = swarm_id_for_session(&req_session_id, swarm_members).await;

    if let Some(swarm_id) = swarm_id {
        let friendly_name = friendly_name_for_session(&req_session_id, swarm_members).await;

        swarm
            .set_shared_context(
                &swarm_id,
                &key,
                value.clone(),
                &req_session_id,
                friendly_name.clone(),
                append,
            )
            .await;

        let swarm_session_ids: Vec<String> = {
            let swarms = swarms_by_id.read().await;
            swarms
                .get(&swarm_id)
                .map(|sessions| sessions.iter().cloned().collect())
                .unwrap_or_default()
        };

        // Shared-context updates are subtree-scoped like broadcasts: notify only
        // the sessions the writer (transitively) spawned, so a share cannot
        // become a member-cap-sized notification storm. The coordinator keeps
        // whole-swarm reach. Everyone can still `read` the key on demand.
        //
        // Compute the target set up front and drop the read guard before the
        // fanout loop: `fanout_session_event` takes a write lock on members.
        let notify_targets: Vec<String> = {
            let members = swarm_members.read().await;
            let sender_is_coordinator = members
                .get(&req_session_id)
                .is_some_and(|member| member.role == "coordinator");
            swarm_session_ids
                .iter()
                .filter(|sid| *sid != &req_session_id)
                .filter(|sid| {
                    sender_is_coordinator
                        || super::swarm_is_self_or_ancestor(&members, &req_session_id, sid)
                })
                .cloned()
                .collect()
        };
        for sid in &notify_targets {
            {
                let _ = fanout_session_event(
                    swarm_members,
                    sid,
                    ServerEvent::Notification {
                        from_session: req_session_id.clone(),
                        from_name: friendly_name.clone(),
                        notification_type: NotificationType::SharedContext {
                            key: key.clone(),
                            value: if append {
                                format!("(appended) {}", value)
                            } else {
                                value.clone()
                            },
                        },
                        message: if append {
                            format!("Appended shared context: {} += {}", key, value)
                        } else {
                            format!("Shared context: {} = {}", key, value)
                        },
                    },
                )
                .await;
            }
        }

        record_swarm_event(
            event_history,
            event_counter,
            swarm_event_tx,
            req_session_id.clone(),
            friendly_name.clone(),
            Some(swarm_id.clone()),
            SwarmEventType::ContextUpdate {
                swarm_id: swarm_id.clone(),
                key: key.clone(),
            },
        )
        .await;

        let _ = client_event_tx.send(ServerEvent::Done { id });
    } else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm. Use a git repository to enable swarm features.".to_string(),
            retry_after_secs: None,
        });
    }
}

pub(super) async fn handle_comm_read(
    id: u64,
    req_session_id: String,
    key: Option<String>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    let swarm_members = &swarm.swarm_state.members;
    let swarm_id = swarm_id_for_session(&req_session_id, swarm_members).await;

    let entries = if let Some(swarm_id) = swarm_id {
        match key {
            Some(k) => swarm
                .get_shared_context(&swarm_id, &k)
                .await
                .map(|c| {
                    vec![ContextEntry {
                        key: c.key,
                        value: c.value,
                        from_session: c.from_session,
                        from_name: c.from_name,
                    }]
                })
                .unwrap_or_default(),
            None => swarm
                .shared_context_entries(&swarm_id)
                .await
                .into_iter()
                .map(|c| ContextEntry {
                    key: c.key,
                    value: c.value,
                    from_session: c.from_session,
                    from_name: c.from_name,
                })
                .collect(),
        }
    } else {
        Vec::new()
    };

    let _ = client_event_tx.send(ServerEvent::CommContext { id, entries });
}

pub(super) async fn handle_comm_list(
    id: u64,
    req_session_id: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
    sessions: &super::SessionAgents,
    client_connections: &Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
) {
    let swarm_members = &swarm.swarm_state.members;
    let swarms_by_id = &swarm.swarm_state.swarms_by_id;
    let file_touch = &swarm.file_touch;
    let swarm_id = swarm_id_for_session(&req_session_id, swarm_members).await;

    if let Some(swarm_id) = swarm_id {
        let swarm_session_ids: Vec<String> = {
            let swarms = swarms_by_id.read().await;
            swarms
                .get(&swarm_id)
                .map(|sessions| sessions.iter().cloned().collect())
                .unwrap_or_default()
        };

        // Snapshot the static member fields first, releasing the members lock
        // before gathering per-session runtime extras (which briefly lock
        // individual agents and read the connection map).
        struct MemberStatic {
            session_id: String,
            friendly_name: Option<String>,
            files: Vec<String>,
            status: String,
            detail: Option<String>,
            task_label: Option<String>,
            role: String,
            is_headless: bool,
            report_back_to_session_id: Option<String>,
            latest_completion_report: Option<String>,
            live_attachments: usize,
            status_age_secs: u64,
        }

        let statics: Vec<MemberStatic> = {
            let members = swarm_members.read().await;
            let touches = file_touch.reverse_snapshot().await;
            swarm_session_ids
                .iter()
                .filter_map(|sid| {
                    members.get(sid).map(|member| {
                        let mut files: Vec<String> = touches
                            .get(sid)
                            .into_iter()
                            .flat_map(|paths| paths.iter())
                            .map(|path| path.display().to_string())
                            .collect();
                        files.sort();
                        MemberStatic {
                            session_id: sid.clone(),
                            friendly_name: member.friendly_name.clone(),
                            files,
                            status: member.status.clone(),
                            detail: member.detail.clone(),
                            task_label: member.task_label.clone(),
                            role: member.role.clone(),
                            is_headless: member.is_headless,
                            report_back_to_session_id: member.report_back_to_session_id.clone(),
                            latest_completion_report: member.latest_completion_report.clone(),
                            live_attachments: member.event_txs.len(),
                            status_age_secs: member.last_status_change.elapsed().as_secs(),
                        }
                    })
                })
                .collect()
        };

        let mut member_list: Vec<AgentInfo> = Vec::with_capacity(statics.len());
        for m in statics {
            let extras = super::comm_sync::member_runtime_extras(
                &m.session_id,
                m.status == "running",
                sessions,
                client_connections,
            )
            .await;

            member_list.push(AgentInfo {
                session_id: m.session_id,
                friendly_name: m.friendly_name,
                files_touched: m.files,
                status: Some(m.status),
                detail: m.detail,
                task_label: m.task_label,
                role: Some(m.role),
                is_headless: Some(m.is_headless),
                report_back_to_session_id: m.report_back_to_session_id,
                latest_completion_report: m.latest_completion_report,
                live_attachments: Some(m.live_attachments),
                status_age_secs: Some(m.status_age_secs),
                last_activity_age_secs: extras.last_activity_age_secs,
                activity: extras.activity,
                provider_name: extras.provider_name,
                provider_model: extras.provider_model,
                provider_effort: extras.provider_effort,
                turn_count: extras.turn_count,
                recent_total_tokens: extras.recent_total_tokens,
                recent_output_tokens: extras.recent_output_tokens,
                recent_window_secs: extras.recent_window_secs,
                cumulative_total_tokens: extras.cumulative_total_tokens,
                todos_completed: extras.todos_completed,
                todos_total: extras.todos_total,
            });
        }

        let _ = client_event_tx.send(ServerEvent::CommMembers {
            id,
            members: member_list,
        });
    } else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm. Use a git repository to enable swarm features.".to_string(),
            retry_after_secs: None,
        });
    }
}
