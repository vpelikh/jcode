use super::services::SwarmServiceHandle;
use super::{
    SwarmEventType, SwarmMember, record_swarm_event, subscribe_session_to_channel,
    unsubscribe_session_from_channel,
};
use crate::protocol::{AgentInfo, ServerEvent, SwarmChannelInfo};
use jcode_swarm_core::ChannelIndex;
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

pub(super) async fn handle_comm_list_channels(
    id: u64,
    req_session_id: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    let swarm_members = &swarm.swarm_state.members;
    let channel_subscriptions = &swarm.channel_subscriptions;
    let swarm_id = swarm_id_for_session(&req_session_id, swarm_members).await;

    if let Some(swarm_id) = swarm_id {
        let channels = {
            let subs = channel_subscriptions.read().await;
            let index = ChannelIndex {
                by_swarm_channel: subs.clone(),
                by_session: HashMap::new(),
            };
            let mut channels: Vec<SwarmChannelInfo> = Vec::new();
            if let Some(swarm_channels) = index.by_swarm_channel.get(&swarm_id) {
                for (channel, members) in swarm_channels {
                    channels.push(SwarmChannelInfo {
                        channel: channel.clone(),
                        member_count: members.len(),
                    });
                }
            }
            channels.sort_by(|left, right| left.channel.cmp(&right.channel));
            channels
        };

        let _ = client_event_tx.send(ServerEvent::CommChannels { id, channels });
    } else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm. Use a git repository to enable swarm features.".to_string(),
            retry_after_secs: None,
        });
    }
}

pub(super) async fn handle_comm_channel_members(
    id: u64,
    req_session_id: String,
    channel: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    let swarm_members = &swarm.swarm_state.members;
    let channel_subscriptions = &swarm.channel_subscriptions;
    let swarm_id = swarm_id_for_session(&req_session_id, swarm_members).await;

    if let Some(swarm_id) = swarm_id {
        let member_ids: Vec<String> = {
            let subs = channel_subscriptions.read().await;
            let index = ChannelIndex {
                by_swarm_channel: subs.clone(),
                by_session: HashMap::new(),
            };
            index.members(&swarm_id, &channel)
        };

        let members = swarm_members.read().await;
        let entries: Vec<AgentInfo> = member_ids
            .iter()
            .filter_map(|sid: &String| {
                members.get(sid).map(|member| AgentInfo {
                    session_id: sid.clone(),
                    friendly_name: member.friendly_name.clone(),
                    files_touched: Vec::new(),
                    status: Some(member.status.clone()),
                    detail: member.detail.clone(),
                    role: Some(member.role.clone()),
                    is_headless: Some(member.is_headless),
                    report_back_to_session_id: member.report_back_to_session_id.clone(),
                    latest_completion_report: member.latest_completion_report.clone(),
                    live_attachments: Some(member.event_txs.len()),
                    status_age_secs: Some(member.last_status_change.elapsed().as_secs()),
                    last_activity_age_secs: crate::session_metrics::last_activity_age_secs(sid),
                    ..Default::default()
                })
            })
            .collect();

        let _ = client_event_tx.send(ServerEvent::CommMembers {
            id,
            members: entries,
        });
    } else {
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm. Use a git repository to enable swarm features.".to_string(),
            retry_after_secs: None,
        });
    }
}

pub(super) async fn handle_comm_subscribe_channel(
    id: u64,
    req_session_id: String,
    channel: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    let swarm_members = &swarm.swarm_state.members;
    let channel_subscriptions = &swarm.channel_subscriptions;
    let channel_subscriptions_by_session = &swarm.channel_subscriptions_by_session;
    let (event_history, event_counter, swarm_event_tx) = swarm.read_event_sources();
    let started = std::time::Instant::now();
    let swarm_id = swarm_id_for_session(&req_session_id, swarm_members).await;

    if let Some(swarm_id) = swarm_id {
        crate::logging::event_info(
            "COMM_LIFECYCLE",
            vec![
                ("phase", "channel_subscribe_start".to_string()),
                ("request_id", id.to_string()),
                ("session_id", req_session_id.clone()),
                ("swarm_id", swarm_id.clone()),
                ("channel", channel.clone()),
            ],
        );
        subscribe_session_to_channel(
            &req_session_id,
            &swarm_id,
            &channel,
            channel_subscriptions,
            channel_subscriptions_by_session,
        )
        .await;

        record_swarm_event(
            event_history,
            event_counter,
            swarm_event_tx,
            req_session_id.clone(),
            None,
            Some(swarm_id.clone()),
            SwarmEventType::Notification {
                notification_type: "channel_subscribe".to_string(),
                message: channel.clone(),
            },
        )
        .await;

        let _ = client_event_tx.send(ServerEvent::Done { id });
        crate::logging::event_info(
            "COMM_LIFECYCLE",
            vec![
                ("phase", "channel_subscribe_done".to_string()),
                ("request_id", id.to_string()),
                ("session_id", req_session_id),
                ("swarm_id", swarm_id),
                ("channel", channel),
                ("elapsed_ms", started.elapsed().as_millis().to_string()),
            ],
        );
    } else {
        crate::logging::event_warn(
            "COMM_LIFECYCLE",
            vec![
                ("phase", "channel_subscribe_error".to_string()),
                ("request_id", id.to_string()),
                ("session_id", req_session_id),
                ("channel", channel),
                ("error", "not_in_swarm".to_string()),
                ("elapsed_ms", started.elapsed().as_millis().to_string()),
            ],
        );
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
    }
}

pub(super) async fn handle_comm_unsubscribe_channel(
    id: u64,
    req_session_id: String,
    channel: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    let swarm_members = &swarm.swarm_state.members;
    let channel_subscriptions = &swarm.channel_subscriptions;
    let channel_subscriptions_by_session = &swarm.channel_subscriptions_by_session;
    let (event_history, event_counter, swarm_event_tx) = swarm.read_event_sources();
    let started = std::time::Instant::now();
    let swarm_id = swarm_id_for_session(&req_session_id, swarm_members).await;

    if let Some(swarm_id) = swarm_id {
        crate::logging::event_info(
            "COMM_LIFECYCLE",
            vec![
                ("phase", "channel_unsubscribe_start".to_string()),
                ("request_id", id.to_string()),
                ("session_id", req_session_id.clone()),
                ("swarm_id", swarm_id.clone()),
                ("channel", channel.clone()),
            ],
        );
        unsubscribe_session_from_channel(
            &req_session_id,
            &swarm_id,
            &channel,
            channel_subscriptions,
            channel_subscriptions_by_session,
        )
        .await;

        record_swarm_event(
            event_history,
            event_counter,
            swarm_event_tx,
            req_session_id.clone(),
            None,
            Some(swarm_id.clone()),
            SwarmEventType::Notification {
                notification_type: "channel_unsubscribe".to_string(),
                message: channel.clone(),
            },
        )
        .await;

        let _ = client_event_tx.send(ServerEvent::Done { id });
        crate::logging::event_info(
            "COMM_LIFECYCLE",
            vec![
                ("phase", "channel_unsubscribe_done".to_string()),
                ("request_id", id.to_string()),
                ("session_id", req_session_id),
                ("swarm_id", swarm_id),
                ("channel", channel),
                ("elapsed_ms", started.elapsed().as_millis().to_string()),
            ],
        );
    } else {
        crate::logging::event_warn(
            "COMM_LIFECYCLE",
            vec![
                ("phase", "channel_unsubscribe_error".to_string()),
                ("request_id", id.to_string()),
                ("session_id", req_session_id),
                ("channel", channel),
                ("error", "not_in_swarm".to_string()),
                ("elapsed_ms", started.elapsed().as_millis().to_string()),
            ],
        );
        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: "Not in a swarm.".to_string(),
            retry_after_secs: None,
        });
    }
}
