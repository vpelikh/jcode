//! Server handlers for the task-DAG mutation ops (seed/expand/complete/inject).
//!
//! These are the live counterparts of the validated engine ops in
//! `jcode_plan::dag`. Each handler lifts the swarm's current `VersionedPlan` into
//! a `TaskGraph` (via `jcode_plan::bridge`), applies the engine op (which enforces
//! acyclicity, ownership, gate insertion, and artifact validation), lowers the
//! result back into the plan, then persists and broadcasts using the existing
//! swarm machinery. This keeps a single source of truth and reuses the scheduler,
//! persistence, and TUI broadcast paths.

use super::services::SwarmServiceHandle;
use super::SwarmMember;
use crate::protocol::ServerEvent;
use crate::protocol::TaskGraphNodeSpec;
use jcode_plan::MAX_PLAN_ITEMS;
use jcode_plan::bridge::{apply_task_graph, parse_kind, to_task_graph};
use jcode_plan::dag::{self, HandoffArtifact, NodeSpec, NodeStatus, TaskGraph};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;

fn spec_from_wire(spec: TaskGraphNodeSpec) -> NodeSpec {
    NodeSpec {
        id: Some(spec.id),
        content: spec.content,
        kind: parse_kind(spec.kind.as_deref()),
        depends_on: spec.depends_on,
        priority: spec.priority,
    }
}

fn graph_size_error(graph: &TaskGraph) -> Option<String> {
    (graph.len() > MAX_PLAN_ITEMS).then(|| {
        format!(
            "plan would contain {} items, exceeding the per-swarm limit of {}; finish or clear stale plan nodes before adding more",
            graph.len(),
            MAX_PLAN_ITEMS
        )
    })
}

async fn swarm_id_for(
    session_id: &str,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<String> {
    swarm_members
        .read()
        .await
        .get(session_id)
        .and_then(|member| member.swarm_id.clone())
}

/// Auto-claim a queued node for the participant that is trying to mutate it.
///
/// Seeded nodes are unowned until dispatch, but the deep-mode contract tells the
/// seeding agent to `expand_node`/`complete_node` its own nodes, and the assign
/// path refuses self-assignment — so without this a solo deep seeder could never
/// legally touch any node it seeded (observed live as "Complete rejected: actor
/// does not own node"). Similarly, assignment to a client-attached worker leaves
/// the item `queued` (the server-run flip to `running` is skipped when a live
/// client owns the turn), so the assignee's own complete/expand would bounce with
/// "invalid state Queued".
///
/// Claiming is safe only when the node is genuinely available to this actor:
/// queued, with every dependency done (enforced by `dispatch`), and either
/// unowned or already assigned to this same actor. A node owned by someone else
/// is never touched — the engine's `NotOwner` check still applies.
fn claim_queued_node_for_actor(graph: &mut TaskGraph, node_id: &str, actor: &str) {
    let claimable = graph.get(node_id).is_some_and(|node| {
        node.status == NodeStatus::Queued
            && node.owner.as_deref().is_none_or(|owner| owner == actor)
    });
    if claimable {
        // `dispatch` re-validates queued status and dependency satisfaction; if
        // deps are not done the claim is skipped and the engine op reports the
        // real error.
        let _ = dag::dispatch(graph, node_id, actor);
    }
}

fn err(client_event_tx: &mpsc::UnboundedSender<ServerEvent>, id: u64, message: String) {
    let _ = client_event_tx.send(ServerEvent::Error {
        id,
        message,
        retry_after_secs: None,
    });
}

/// Seed (or re-seed) the swarm task DAG from a batch of node specs.
pub(super) async fn handle_comm_seed_graph(
    id: u64,
    req_session_id: String,
    mode: Option<String>,
    nodes: Vec<TaskGraphNodeSpec>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    // Swarm-domain state is reached through the swarm service handle. These
    // locals keep the body single-homed on the handle's fields (server service
    // split, convergence slice).
    let swarm_members = &swarm.swarm_state().members;
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };

    // A deep-mode seeder is usually a solo agent. Elect it coordinator (when no
    // live coordinator exists) so it can actually dispatch the graph it seeds via
    // the coordinator-gated assign/run_plan paths.
    swarm
        .elect_seeder_coordinator(&swarm_id, &req_session_id)
        .await;

    let specs: Vec<NodeSpec> = nodes.into_iter().map(spec_from_wire).collect();
    let count = specs.len();

    // Resolve the plan mode. The model is *asked* to pass `mode:"deep"` when it is
    // running at `swarm-deep` effort, but it frequently forgets. Rather than
    // silently downgrading a deep-effort session to light (which disables the
    // gates + artifact validation that define deep mode), default the mode from
    // the seeder's recorded reasoning effort when the caller did not specify one.
    // An explicit `mode` always wins so a caller can still opt into light.
    let resolved_mode = mode.or_else(|| {
        crate::session_effort::session_effort(&req_session_id)
            .filter(|effort| crate::prompt::is_deep_swarm_effort(effort))
            .map(|_| "deep".to_string())
    });

    swarm
        .mutate_task_dag(
            id,
            &swarm_id,
            &req_session_id,
            "task_graph_seed",
            count,
            client_event_tx,
            true,
            |plan| {
                if let Some(mode) = &resolved_mode {
                    // Guard against silent rigor downgrades: re-seeding an existing deep
                    // plan as light would strip the gates + artifact validation from all
                    // nodes already in flight. Deepening (light -> deep) or re-stating
                    // the same mode is fine; only the downgrade of a non-empty deep plan
                    // is rejected.
                    let downgrades_deep = plan.mode.eq_ignore_ascii_case("deep")
                        && !mode.eq_ignore_ascii_case("deep")
                        && !plan.items.is_empty();
                    if downgrades_deep {
                        return Err(
                            "Seed rejected: this swarm already has a non-empty deep-mode plan; \
                             seeding with mode=light would silently strip its gates and artifact \
                             validation. Omit `mode` to keep deep, or finish/clear the current plan first."
                                .to_string(),
                        );
                    }
                    plan.mode = mode.clone();
                }
                plan.participants.insert(req_session_id.clone());
                let mut graph = to_task_graph(plan);
                let before = graph.clone();
                match dag::seed(&mut graph, specs) {
                    Ok(()) => match graph_size_error(&graph) {
                        Some(message) => Err(format!("Seed rejected: {message}")),
                        None => {
                            if graph != before {
                                apply_task_graph(plan, &graph);
                                plan.version += 1;
                            }
                            Ok(())
                        }
                    },
                    Err(e) => Err(format!("Seed rejected: {e}")),
                }
            },
        )
        .await;
}

/// Decompose a node the caller owns into a child sub-DAG.
pub(super) async fn handle_comm_expand_node(
    id: u64,
    req_session_id: String,
    node_id: String,
    children: Vec<TaskGraphNodeSpec>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    // Swarm-domain state is reached through the swarm service handle. These
    // locals keep the body single-homed on the handle's fields (server service
    // split, convergence slice).
    let swarm_members = &swarm.swarm_state().members;
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };
    let specs: Vec<NodeSpec> = children.into_iter().map(spec_from_wire).collect();
    let count = specs.len();

    swarm
        .mutate_task_dag(
            id,
            &swarm_id,
            &req_session_id,
            "task_graph_expand",
            count,
            client_event_tx,
            false,
            |plan| {
                let mut graph = to_task_graph(plan);
                claim_queued_node_for_actor(&mut graph, &node_id, &req_session_id);
                match dag::expand_node(&mut graph, &node_id, &req_session_id, specs) {
                    Ok(_) => match graph_size_error(&graph) {
                        Some(message) => Err(format!("Expand rejected: {message}")),
                        None => {
                            apply_task_graph(plan, &graph);
                            plan.version += 1;
                            Ok(())
                        }
                    },
                    Err(e) => Err(format!("Expand rejected: {e}")),
                }
            },
        )
        .await;
}

/// Complete a node the caller owns with a typed handoff artifact.
pub(super) async fn handle_comm_complete_node(
    id: u64,
    req_session_id: String,
    node_id: String,
    artifact_json: String,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    // Swarm-domain state is reached through the swarm service handle. These
    // locals keep the body single-homed on the handle's fields (server service
    // split, convergence slice).
    let swarm_members = &swarm.swarm_state().members;
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };

    let artifact: HandoffArtifact = match serde_json::from_str(&artifact_json) {
        Ok(artifact) => artifact,
        Err(e) => {
            err(client_event_tx, id, format!("Invalid artifact JSON: {e}"));
            return;
        }
    };

    swarm
        .mutate_task_dag(
            id,
            &swarm_id,
            &req_session_id,
            "task_graph_complete",
            1,
            client_event_tx,
            false,
            |plan| {
                let mut graph = to_task_graph(plan);
                claim_queued_node_for_actor(&mut graph, &node_id, &req_session_id);
                match dag::complete_node(&mut graph, &node_id, &req_session_id, artifact) {
                    Ok(()) => {
                        apply_task_graph(plan, &graph);
                        plan.version += 1;
                        Ok(())
                    }
                    Err(e) => Err(format!("Complete rejected: {e}")),
                }
            },
        )
        .await;
}

/// Inject gap/fix nodes from a gate the caller owns.
pub(super) async fn handle_comm_inject_gap(
    id: u64,
    req_session_id: String,
    gate_id: String,
    nodes: Vec<TaskGraphNodeSpec>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    swarm: &SwarmServiceHandle,
) {
    // Swarm-domain state is reached through the swarm service handle. These
    // locals keep the body single-homed on the handle's fields (server service
    // split, convergence slice).
    let swarm_members = &swarm.swarm_state().members;
    let Some(swarm_id) = swarm_id_for(&req_session_id, swarm_members).await else {
        err(client_event_tx, id, "Not in a swarm.".to_string());
        return;
    };
    let specs: Vec<NodeSpec> = nodes.into_iter().map(spec_from_wire).collect();
    let count = specs.len();

    swarm
        .mutate_task_dag(
            id,
            &swarm_id,
            &req_session_id,
            "task_graph_inject_gap",
            count,
            client_event_tx,
            false,
            |plan| {
                let mut graph = to_task_graph(plan);
                claim_queued_node_for_actor(&mut graph, &gate_id, &req_session_id);
                match dag::inject_from_gate(&mut graph, &gate_id, &req_session_id, specs) {
                    Ok(_) => match graph_size_error(&graph) {
                        Some(message) => Err(format!("Inject rejected: {message}")),
                        None => {
                            apply_task_graph(plan, &graph);
                            plan.version += 1;
                            Ok(())
                        }
                    },
                    Err(e) => Err(format!("Inject rejected: {e}")),
                }
            },
        )
        .await;
}
