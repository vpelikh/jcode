use chrono::{DateTime, Utc};
use jcode_session_types::{StoredCompactionState, StoredMemoryInjection, StoredMessage};
use crate::session::model::StoredReplayEvent;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// Maximum acceptable skew between an event's timestamp and "now" when
/// validating an event at insert time. Generous enough for normal sessions,
/// but rejects wildly improbable clocks. (Forking and rehydration use
/// unvalidated `push_event`, so this never drops already-trusted history.)
const MAX_EVENT_AGE_SECS: i64 = 86400 * 365; // ~1 year

/// Errors that can occur when working with session events
#[derive(Debug, Clone)]
pub enum SessionEventError {
    /// Event ID is invalid or malformed
    InvalidEventId { event_id: String },
    /// Event timestamp is invalid (too far in future or past)
    InvalidTimestamp { timestamp: DateTime<Utc> },
    /// Message content is invalid
    InvalidMessageContent { message_id: String },
    /// Compaction state is invalid
    InvalidCompactionState { reason: String },
    /// Memory injection data is invalid
    InvalidMemoryInjection { reason: String },
}

impl fmt::Display for SessionEventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionEventError::InvalidEventId { event_id } => {
                write!(f, "Invalid event ID: {}", event_id)
            }
            SessionEventError::InvalidTimestamp { timestamp } => {
                write!(f, "Invalid timestamp: {}", timestamp)
            }
            SessionEventError::InvalidMessageContent { message_id } => {
                write!(f, "Invalid message content for message ID: {}", message_id)
            }
            SessionEventError::InvalidCompactionState { reason } => {
                write!(f, "Invalid compaction state: {}", reason)
            }
            SessionEventError::InvalidMemoryInjection { reason } => {
                write!(f, "Invalid memory injection: {}", reason)
            }
        }
    }
}

impl Error for SessionEventError {}

/// Surface operations for events in the SessionEventMap.
///
/// The enum is deliberately **extensible via an escape hatch**, not closed at
/// definition time. Rust has no declaration-merging counterpart to
/// deepseek-harness's derived-union events, so unknown `op` tags deserialize
/// into [`SessionEventOp::Unknown`] with their full payload preserved, matching
/// how a future plugin event type would be carried through the log.
///
/// **Wire format (uniform envelope).** Every event serializes as
/// `{"op": <tag>, "data": <payload>}`. For a known variant `<payload>` holds
/// its field set (as a JSON object); for [`SessionEventOp::Unknown`] `<payload>`
/// is the plugin's verbatim payload and may be any JSON value (object, scalar,
/// or array). Because the payload always lives under `data` — nested, never at
/// the tag level — there is **no reserved-key collision**: a plugin payload may
/// safely contain a top-level field named `op`, and a non-object payload needs
/// no bundling heuristic. Both earlier `Unknown` trade-offs are eliminated.
///
/// The extension rule: **add an event, don't edit the loop** — a plugin that
/// needs a custom event appends an `Unknown { event_type, data }` event rather
/// than being blocked on a release of the core enum.
#[derive(Debug, Clone)]
pub enum SessionEventOp {
    /// Append a new message to the session
    AppendMessage {
        message_id: String,
        message: StoredMessage,
    },
    /// Replace messages in a range
    ReplaceMessages {
        start_index: usize,
        end_index: usize,
        messages: Vec<StoredMessage>,
    },
    /// Insert a message at a specific index
    InsertMessage {
        index: usize,
        message: StoredMessage,
    },
    /// Record a memory injection event
    MemoryInjection {
        memory_injection: StoredMemoryInjection,
    },
    /// Record a replay event
    ReplayEvent {
        replay_event: StoredReplayEvent,
    },
    /// Set compaction state (replaces existing)
    SetCompaction {
        compaction: StoredCompactionState,
    },
    /// Open a compaction bracket (a log marker / lock). Compaction is a
    /// **log-bracketed, replayable operation** (takeaway #5): a run appends
    /// `CompactionStart`, performs the span replace + summary, then appends
    /// `CompactionEnd`. A crash between `CompactionStart` and `CompactionEnd`
    /// leaves a *detectable orphaned lock* rather than a half-applied summary,
    /// so replay can stop at the orphan marker and surface an explicit
    /// "incomplete compaction" state instead of silently corrupting the surface.
    ///
    /// Carries the compaction id / boundary being consolidated so a crash that
    /// happens mid-run can be tied back to which span was being summarized.
    CompactionStart {
        compaction_id: String,
        /// Number of turns being summarized in this bracket (diagnostic).
        covers_up_to_turn: usize,
    },
    /// Close a compaction bracket, persisting the resulting compaction state.
    /// Only a matching open [`SessionEventOp::CompactionStart`] may be closed;
    /// appending `CompactionEnd` without a preceding open marker is an orphan
    /// and must be detected (see [`SessionEventMap::orphaned_compaction`]).
    CompactionEnd {
        compaction: StoredCompactionState,
    },
    /// Clear all messages (full replacement)
    ClearAll,
    /// An event op the core enum does not know about.
    ///
    /// This is the escape hatch (takeaway #13): unknown `op` tags deserialize
    /// here instead of erroring, so a future plugin can add an event kind
    /// without a breaking change to this enum. `event_type` is the raw `op`
    /// string; `data` is the plugin's verbatim payload, carried under the
    /// envelope's `data` field so it never collides with the tag. Serialization
    /// re-emits `{"op": <event_type>, "data": <data>}` losslessly for any JSON
    /// payload (object, scalar, or array).
    Unknown {
        /// The raw `op` tag as it appeared on the wire.
        event_type: String,
        /// The plugin payload, stored verbatim under the envelope's `data` key.
        data: serde_json::Value,
    },
}

/// The `op` discriminator key used by [`SessionEventOp`] on the wire and on
/// disk. Kept in one place so the manual (de)serializers stay in sync.
const OP_KEY: &str = "op";

impl SessionEventOp {
    /// Render the `op` discriminator for each known variant, mirroring the
    /// previous `#[serde(rename_all = "snake_case")]` on the tag.
    fn known_op_tag(&self) -> Option<&'static str> {
        match self {
            SessionEventOp::AppendMessage { .. } => Some("append_message"),
            SessionEventOp::ReplaceMessages { .. } => Some("replace_messages"),
            SessionEventOp::InsertMessage { .. } => Some("insert_message"),
            SessionEventOp::MemoryInjection { .. } => Some("memory_injection"),
            SessionEventOp::ReplayEvent { .. } => Some("replay_event"),
            SessionEventOp::SetCompaction { .. } => Some("set_compaction"),
            SessionEventOp::CompactionStart { .. } => Some("compaction_start"),
            SessionEventOp::CompactionEnd { .. } => Some("compaction_end"),
            SessionEventOp::ClearAll => Some("clear_all"),
            SessionEventOp::Unknown { .. } => None,
        }
    }
}

impl Serialize for SessionEventOp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        // Uniform envelope: `{"op": <tag>, "data": <payload>}`. The payload is
        // always nested under `data`, so a payload field named `op` never
        // collides with the tag (no reserved-key dropping) and a non-object
        // payload needs no bundling heuristic.
        let tag = match self {
            SessionEventOp::Unknown { event_type, .. } => event_type.clone(),
            _ => self
                .known_op_tag()
                .expect("known variants always have a tag")
                .to_string(),
        };
        let payload: serde_json::Value = match self {
            SessionEventOp::Unknown { data, .. } => data.clone(),
            other => {
                // Build the field set for a known variant as a JSON object. The
                // fields are emitted in the map's iteration order (alphabetical,
                // as serde_json has no `preserve_order`). `to_value` on an
                // *individual field* is safe because fields are plain data types
                // (no recursive `SessionEventOp`).
                let mut obj = serde_json::Map::new();
                match other {
                    SessionEventOp::AppendMessage { message_id, message } => {
                        obj.insert("message_id".into(), serde_json::to_value(message_id).map_err(serde::ser::Error::custom)?);
                        obj.insert("message".into(), serde_json::to_value(message).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::ReplaceMessages { start_index, end_index, messages } => {
                        obj.insert("start_index".into(), serde_json::to_value(start_index).map_err(serde::ser::Error::custom)?);
                        obj.insert("end_index".into(), serde_json::to_value(end_index).map_err(serde::ser::Error::custom)?);
                        obj.insert("messages".into(), serde_json::to_value(messages).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::InsertMessage { index, message } => {
                        obj.insert("index".into(), serde_json::to_value(index).map_err(serde::ser::Error::custom)?);
                        obj.insert("message".into(), serde_json::to_value(message).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::MemoryInjection { memory_injection } => {
                        obj.insert("memory_injection".into(), serde_json::to_value(memory_injection).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::ReplayEvent { replay_event } => {
                        obj.insert("replay_event".into(), serde_json::to_value(replay_event).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::SetCompaction { compaction } => {
                        obj.insert("compaction".into(), serde_json::to_value(compaction).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::CompactionStart { compaction_id, covers_up_to_turn } => {
                        obj.insert("compaction_id".into(), serde_json::to_value(compaction_id).map_err(serde::ser::Error::custom)?);
                        obj.insert("covers_up_to_turn".into(), serde_json::to_value(covers_up_to_turn).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::CompactionEnd { compaction } => {
                        obj.insert("compaction".into(), serde_json::to_value(compaction).map_err(serde::ser::Error::custom)?);
                    }
                    SessionEventOp::ClearAll => {}
                    SessionEventOp::Unknown { .. } => unreachable!("Unknown handled above"),
                }
                serde_json::Value::Object(obj)
            }
        };
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry(OP_KEY, &tag)?;
        map.serialize_entry("data", &payload)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for SessionEventOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Parse into an untagged map first so we can inspect the `op` tag and
        // read the nested `data` payload.
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("SessionEventOp must be a JSON object"))?;

        let op = obj
            .get(OP_KEY)
            .and_then(|v| v.as_str())
            .ok_or_else(|| serde::de::Error::custom(format!("SessionEventOp is missing a string '{OP_KEY}' tag")))?;

        // The payload is always nested under `data`. For a known variant it is
        // the variant's field object; for an `Unknown` it is the verbatim plugin
        // value (any JSON). Because the payload lives under `data`, a payload
        // field named `op` never collides with the tag and a non-object payload
        // is unambiguous — no unwrapping heuristic needed.
        let data = obj
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // Forward-compat fallback for the escape hatch: a known `op` tag may be a
        // FUTURE core variant whose payload shape differs from what this build
        // expects. If the per-variant parse fails, degrade to `Unknown`
        // (preserving the full payload) rather than erroring — otherwise a single
        // shape mismatch on one event would fail to load the whole journal.
        let make_unknown = |event_type: &str| -> SessionEventOp {
            SessionEventOp::Unknown {
                event_type: event_type.to_string(),
                data: data.clone(),
            }
        };

        match op {
            "append_message" => {
                match serde_json::from_value::<AppendMessageFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::AppendMessage {
                        message_id: fields.message_id,
                        message: fields.message,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "replace_messages" => {
                match serde_json::from_value::<ReplaceMessagesFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::ReplaceMessages {
                        start_index: fields.start_index,
                        end_index: fields.end_index,
                        messages: fields.messages,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "insert_message" => {
                match serde_json::from_value::<InsertMessageFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::InsertMessage {
                        index: fields.index,
                        message: fields.message,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "memory_injection" => {
                match serde_json::from_value::<MemoryInjectionFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::MemoryInjection {
                        memory_injection: fields.memory_injection,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "replay_event" => {
                match serde_json::from_value::<ReplayEventFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::ReplayEvent {
                        replay_event: fields.replay_event,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "set_compaction" => {
                match serde_json::from_value::<SetCompactionFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::SetCompaction {
                        compaction: fields.compaction,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "compaction_start" => {
                match serde_json::from_value::<CompactionStartFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::CompactionStart {
                        compaction_id: fields.compaction_id,
                        covers_up_to_turn: fields.covers_up_to_turn,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "compaction_end" => {
                match serde_json::from_value::<CompactionEndFields>(data.clone()) {
                    Ok(fields) => Ok(SessionEventOp::CompactionEnd {
                        compaction: fields.compaction,
                    }),
                    Err(_) => Ok(make_unknown(op)),
                }
            }
            "clear_all" => {
                // The unit variant serializes as `{"op":"clear_all","data":{}}`.
                // It carries no fields; tolerate any payload defensively.
                Ok(SessionEventOp::ClearAll)
            }
            // Unknown `op`: escape hatch. Preserve the type tag and every piece
            // of payload verbatim so a plugin event round-trips losslessly and
            // future core releases can promote it without losing logged data.
            other => Ok(make_unknown(other)),
        }
    }
}

/// Transient helper structs used by the manual `Deserialize` impl to reuse the
/// derived per-field deserialization for each known variant (the field object
/// lives under the envelope's `data` key, so these structs describe the payload
/// with no `op` tag).
#[derive(Deserialize)]
struct AppendMessageFields {
    message_id: String,
    message: StoredMessage,
}
#[derive(Deserialize)]
struct ReplaceMessagesFields {
    start_index: usize,
    end_index: usize,
    messages: Vec<StoredMessage>,
}
#[derive(Deserialize)]
struct InsertMessageFields {
    index: usize,
    message: StoredMessage,
}
#[derive(Deserialize)]
struct MemoryInjectionFields {
    memory_injection: StoredMemoryInjection,
}
#[derive(Deserialize)]
struct ReplayEventFields {
    replay_event: StoredReplayEvent,
}
#[derive(Deserialize)]
struct SetCompactionFields {
    compaction: StoredCompactionState,
}
#[derive(Deserialize)]
struct CompactionStartFields {
    compaction_id: String,
    covers_up_to_turn: usize,
}
#[derive(Deserialize)]
struct CompactionEndFields {
    compaction: StoredCompactionState,
}

/// A single event in the session event log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub timestamp: DateTime<Utc>,
    pub event_id: String,
    pub op: SessionEventOp,
    /// Optional parent event ID for merge-extensibility
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Version for conflict resolution
    pub version: u64,
}

/// A merge-extensible event map - the single source of truth for session state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionEventMap {
    /// All events in append-only order
    pub events: Vec<SessionEvent>,
    /// Cache of the most recent SetCompaction event, for O(1) current_compaction.
    /// Deliberately NOT persisted (`serde(skip)`): `events` is the sole
    /// authority, and a persisted cache would become a second authoritative
    /// state that could silently diverge from `events` on direct mutation —
    /// exactly the dual-source hazard this event log exists to avoid.
    /// `current_compaction` therefore reverse-scans `events` when the cache is
    /// empty (e.g. after deserialization). This bounds the cost to O(n) on a
    /// fresh load rather than risking a stale cache.
    #[serde(skip)]
    cached_compaction: Option<StoredCompactionState>,
}

impl SessionEventMap {
    /// True when the log holds no committed events. Used to keep empty logs out
    /// of a serialized snapshot (a session with no events round-trips in the
    /// historical byte format).
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Update the compaction cache when appending an event (SetCompaction /
    /// CompactionEnd persist compaction; ClearAll clears it).
    fn update_caches(&mut self, event: &SessionEvent) {
        match &event.op {
            SessionEventOp::SetCompaction { compaction }
            | SessionEventOp::CompactionEnd { compaction } => {
                self.cached_compaction = Some(compaction.clone());
            }
            SessionEventOp::ClearAll => {
                // Any cached compaction state refers to messages that no longer
                // exist after a clear; drop it so current_compaction() reflects
                // the cleared transcript.
                self.cached_compaction = None;
            }
            _ => {}
        }
    }

    /// Append a new event to the map.
    ///
    /// Events are validated before insertion. Invalid events are skipped with a
    /// stderr diagnostic (the event log must stay append-only and corruption-tolerant).
    pub fn append_event(&mut self, event: SessionEvent) {
        if let Err(err) = Self::validate_event(&event) {
            eprintln!("session_event: skipping invalid event {}: {}", event.event_id, err);
            return;
        }
        self.update_caches(&event);
        self.events.push(event);
    }

    /// Append without validation — used for rehydration from trusted legacy vectors.
    pub(crate) fn push_event(&mut self, event: SessionEvent) {
        self.update_caches(&event);
        self.events.push(event);
    }
    
    /// Derive current messages from events
    pub fn derive_messages(&self) -> Vec<StoredMessage> {
        let mut messages = Vec::new();
        
        for event in &self.events {
            match &event.op {
                SessionEventOp::AppendMessage { message, .. } => {
                    messages.push(message.clone());
                }
                SessionEventOp::InsertMessage { index, message, .. } => {
                    // `Vec::insert` accepts `index == len` (append-at-end); the
                    // legacy `insert_message` path uses it directly. Excluding
                    // that case would drop an end-append from the derived
                    // transcript and desync the log from `self.messages`.
                    // Clamp any larger index (defensive only, the legacy path
                    // cannot produce it without panicking) to len.
                    let idx = (*index).min(messages.len());
                    messages.insert(idx, message.clone());
                }
                SessionEventOp::ReplaceMessages { start_index, end_index, messages: replace_with, .. } => {
                    // Clamp so the splice is always valid. `start_index` may
                    // equal the current length (a replacement issued after the
                    // transcript was cleared/truncated to empty must append
                    // rather than be silently dropped), and `end_index` may be
                    // usize::MAX for a full replacement.
                    //
                    // Protect against a *reversed* span where `start_index >
                    // end_index`: `Vec::splice(start..end, ..)` panics when
                    // `start > end`. Each bound is clamped independently to the
                    // live length first, then `end` is forced to be >= `start`
                    // so a reversed span degrades to `start == end` — a
                    // point-insertion at `start` (matching equal-bounds
                    // semantics), never a crash. Producers never emit reversed
                    // bounds through the current API, but the log is
                    // corruption-tolerant by design and must not panic on a
                    // malformed event.
                    let start = (*start_index).min(messages.len());
                    let raw_end = (*end_index).min(messages.len());
                    let end = raw_end.max(start);
                    messages.splice(start..end, replace_with.clone());
                }
                SessionEventOp::ClearAll => {
                    messages.clear();
                }
                _ => {}
            }
        }
        
        messages
    }
    
    /// Get current compaction state
    pub fn current_compaction(&self) -> Option<StoredCompactionState> {
        if let Some(cached) = &self.cached_compaction {
            return Some(cached.clone());
        }
        // Fallback: the in-memory cache is empty after deserialization (`serde(skip)`)
        // or a direct `events` mutation that bypassed `update_caches`. Scan
        // events in reverse to find the most recent SetCompaction / CompactionEnd
        // (the persisting op of a bracketed compaction). If a ClearAll appears
        // after that, the compaction is considered cleared and we return None.
        // `events` is the sole authority, so this scan is always correct.
        let mut found_compaction = None;
        for event in self.events.iter().rev() {
            match &event.op {
                SessionEventOp::SetCompaction { compaction }
                | SessionEventOp::CompactionEnd { compaction } => {
                    found_compaction = Some(compaction.clone());
                    break;
                }
                SessionEventOp::ClearAll => {
                    // ClearAll after the latest compaction clears it
                    return None;
                }
                _ => {}
            }
        }
        found_compaction
    }
    
    /// Get memory injections from events
    pub fn memory_injections(&self) -> Vec<StoredMemoryInjection> {
        let mut injections = Vec::new();
        
        for event in &self.events {
            if let SessionEventOp::MemoryInjection { memory_injection } = &event.op {
                injections.push(memory_injection.clone());
            }
        }
        
        injections
    }
    
    /// Get replay events from events
    pub fn replay_events(&self) -> Vec<StoredReplayEvent> {
        let mut events = Vec::new();
        
        for event in &self.events {
            if let SessionEventOp::ReplayEvent { replay_event } = &event.op {
                events.push(replay_event.clone());
            }
        }
        
        events
    }
    
    /// Fork the event map up to a boundary
    pub fn fork_up_to_boundary(&self, boundary_index: usize) -> Self {
        let mut fork = SessionEventMap::default();
        
        for (event_index, event) in self.events.iter().enumerate() {
            if event_index <= boundary_index {
                // Use push_event (no validation): the events being forked were
                // already validated when originally appended to this log, and
                // forking must not drop them merely because they carry their
                // original timestamps (e.g. a session older than the validation
                // window). Re-validating here would otherwise silently truncate
                // the fork of long-lived or imported sessions.
                fork.push_event(event.clone());
            }
        }
        
        fork
    }
    
    /// Re-derive all state (pure computation)
    pub fn rederive_all(&self) -> (Vec<StoredMessage>, Option<StoredCompactionState>) {
        let messages = self.derive_messages();
        let compaction = self.current_compaction();
        (messages, compaction)
    }

    /// Detect an **orphaned compaction lock**.
    ///
    /// Compaction is a log-bracketed operation (`CompactionStart` … `CompactionEnd`).
    /// If the log ends with an open `CompactionStart` that was never closed, a
    /// crash happened mid-summarize: the surface may be half-applied. This
    /// returns that open bracket so replay can stop there and surface an
    /// explicit "incomplete compaction" state (takeaway #5) instead of silently
    /// treating the partial result as complete.
    ///
    /// An orphan is any `CompactionStart` whose `CompactionEnd` never appears
    /// later in the log. The **latest** such open bracket is what a resumed
    /// session needs to resolve.
    pub fn orphaned_compaction(&self) -> Option<&SessionEvent> {
        // Track the most recent start that has not been matched by a later end.
        // A LIFO stack: each `CompactionEnd` pops the innermost open `Start`,
        // so an unmatched `Start` that is nested *below* a closed bracket is
        // reported correctly (a naive depth counter would wrongly keep the
        // closed inner `Start` around when a later `End` pops the depth back
        // past zero). Compaction runs are non-nesting in practice, but the log
        // is corruption-tolerant by design and must still identify the true
        // orphan on a malformed sequence.
        let mut open: Vec<&SessionEvent> = Vec::new();
        for event in &self.events {
            match &event.op {
                SessionEventOp::CompactionStart { .. } => {
                    open.push(event);
                }
                SessionEventOp::CompactionEnd { .. } => {
                    open.pop();
                }
                _ => {}
            }
        }
        // The innermost (latest) unmatched Start is what a resumed session needs
        // to resolve. `open` is non-empty exactly when a bracket is orphaned.
        // Use [`orphaned_compactions`](Self::orphaned_compactions) to enumerate
        // *all* unmatched open brackets, which is needed when a malformed log has
        // more than one.
        open.last().copied()
    }

    /// Enumerate **every** orphaned compaction bracket in the log, not just the
    /// innermost. Returns the unmatched `CompactionStart` events (in log order).
    ///
    /// `orphaned_compaction()` surfaces only the latest unmatched `Start` for
    /// crash-recovery; this method is the complete view. A log with several
    /// unclosed brackets (e.g. after a sequence of interrupted runs) has them
    /// all reported, so a resumed session or the `CompactionBracket` invariant
    /// can resolve or report every open marker rather than only the last one.
    pub fn orphaned_compactions(&self) -> Vec<&SessionEvent> {
        let mut open: Vec<&SessionEvent> = Vec::new();
        for event in &self.events {
            match &event.op {
                SessionEventOp::CompactionStart { .. } => {
                    open.push(event);
                }
                SessionEventOp::CompactionEnd { .. } => {
                    open.pop();
                }
                _ => {}
            }
        }
        open
    }

    /// Open a compaction bracket by appending a `CompactionStart` marker.
    pub fn start_compaction(&mut self, compaction_id: impl Into<String>, covers_up_to_turn: usize) {
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: crate::id::new_id("compaction_start"),
            op: SessionEventOp::CompactionStart {
                compaction_id: compaction_id.into(),
                covers_up_to_turn,
            },
            parent_id: None,
            version: 1,
        };
        self.append_event(event);
    }

    /// Close a compaction bracket by appending a `CompactionEnd` that persists
    /// the resulting state. `Self::append_event` validation catches a malformed
    /// state; callers should also verify `orphaned_compaction` was open before
    /// closing (a closing `CompactionEnd` without an open start is itself an
    /// orphan the invariant registry flags).
    pub fn end_compaction(&mut self, compaction: StoredCompactionState) {
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: crate::id::new_id("compaction_end"),
            op: SessionEventOp::CompactionEnd {
                compaction: compaction.clone(),
            },
            parent_id: None,
            version: 1,
        };
        self.append_event(event);
    }
    
    /// Validate an event before adding to the map
    ///
    /// # Arguments
    /// * `event` - The event to validate
    ///
    /// # Returns
    /// Ok(()) if event is valid, Err(SessionEventError) otherwise
    fn validate_event(event: &SessionEvent) -> Result<(), SessionEventError> {
        // Validate event_id
        if event.event_id.is_empty() {
            return Err(SessionEventError::InvalidEventId {
                event_id: event.event_id.clone()
            });
        }
        
        // Validate timestamp (not too far in future or past)
        let now = chrono::Utc::now();
        let timestamp_diff = (event.timestamp - now).num_seconds().abs();
        if timestamp_diff > MAX_EVENT_AGE_SECS {
            return Err(SessionEventError::InvalidTimestamp {
                timestamp: event.timestamp
            });
        }
        
        // Validate operation based on event type
        match &event.op {
            SessionEventOp::AppendMessage { message, .. } => {
                Self::validate_message(message, &event.event_id)?;
            }
            SessionEventOp::InsertMessage { message, .. } => {
                Self::validate_message(message, &event.event_id)?;
            }
            SessionEventOp::SetCompaction { compaction } => {
                Self::validate_compaction(compaction)?;
            }
            SessionEventOp::CompactionStart { covers_up_to_turn, .. } => {
                if *covers_up_to_turn == 0 {
                    return Err(SessionEventError::InvalidCompactionState {
                        reason: "compaction bracket covers zero turns".to_string(),
                    });
                }
            }
            SessionEventOp::CompactionEnd { compaction } => {
                Self::validate_compaction(compaction)?;
            }
            SessionEventOp::MemoryInjection { memory_injection } => {
                Self::validate_memory_injection(memory_injection)?;
            }
            SessionEventOp::ReplayEvent { replay_event } => {
                Self::validate_replay_event(replay_event)?;
            }
            // The escape hatch must still carry a meaningful discriminator. An
            // empty `op` tag would produce an event the log cannot later promote
            // or route (it matches no known variant and names no future plugin),
            // so reject it like an empty event_id.
            SessionEventOp::Unknown { event_type, .. } => {
                if event_type.is_empty() {
                    return Err(SessionEventError::InvalidEventId {
                        event_id: "<unknown op>".to_string(),
                    });
                }
            }
            _ => {} // Other operations don't need additional validation
        }
        
        Ok(())
    }
    
    /// Validate a message content
    fn validate_message(message: &StoredMessage, message_id: &str) -> Result<(), SessionEventError> {
        if message.id.is_empty() && message_id.is_empty() {
            return Err(SessionEventError::InvalidMessageContent {
                message_id: message_id.to_string()
            });
        }
        // A message with no content blocks carries no signal (neither text nor
        // tool use/result); refuse to record it so the log stays meaningful.
        if message.content.is_empty() {
            return Err(SessionEventError::InvalidMessageContent {
                message_id: if message.id.is_empty() { message_id.to_string() } else { message.id.clone() }
            });
        }
        Ok(())
    }
    
    /// Validate compaction state
    pub(crate) fn validate_compaction(compaction: &StoredCompactionState) -> Result<(), SessionEventError> {
        if compaction.covers_up_to_turn > compaction.original_turn_count {
            return Err(SessionEventError::InvalidCompactionState {
                reason: format!(
                    "covers_up_to_turn ({}) cannot be greater than original_turn_count ({})",
                    compaction.covers_up_to_turn, compaction.original_turn_count
                )
            });
        }
        
        if compaction.compacted_count > compaction.original_turn_count {
            return Err(SessionEventError::InvalidCompactionState {
                reason: format!(
                    "compacted_count ({}) cannot be greater than original_turn_count ({})",
                    compaction.compacted_count, compaction.original_turn_count
                )
            });
        }
        
        Ok(())
    }
    
    /// Validate memory injection data
    fn validate_memory_injection(injection: &StoredMemoryInjection) -> Result<(), SessionEventError> {
        if injection.content.is_empty() {
            return Err(SessionEventError::InvalidMemoryInjection {
                reason: "Memory injection content cannot be empty".to_string()
            });
        }
        
        // Additional validation can be added here
        Ok(())
    }
    
    /// Validate replay event
    fn validate_replay_event(replay_event: &StoredReplayEvent) -> Result<(), SessionEventError> {
        if replay_event.timestamp > chrono::Utc::now() {
            return Err(SessionEventError::InvalidTimestamp {
                timestamp: replay_event.timestamp
            });
        }

        Ok(())
    }
}
