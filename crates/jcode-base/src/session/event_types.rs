use chrono::{DateTime, Utc};
use jcode_session_types::{StoredCompactionState, StoredMemoryInjection, StoredMessage};
use crate::session::model::StoredReplayEvent;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// Errors that can occur when working with session events
#[derive(Debug, Clone)]
pub enum SessionEventError {
    /// Event ID is invalid or malformed
    InvalidEventId { event_id: String },
    /// Event index is out of bounds
    IndexOutOfBounds { index: usize, max: usize },
    /// Event operation is invalid in current context
    InvalidOperation { op: String },
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
            SessionEventError::IndexOutOfBounds { index, max } => {
                write!(f, "Index {} is out of bounds (max: {})", index, max)
            }
            SessionEventError::InvalidOperation { op } => {
                write!(f, "Invalid operation: {}", op)
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

/// Surface operations for events in the SessionEventMap
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
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
    /// Clear all messages (full replacement)
    ClearAll,
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
    #[serde(skip)]
    compaction_event_index: Option<SessionEvent>,
}

impl SessionEventMap {
    /// Append a new event to the map.
    ///
    /// Events are validated before insertion. Invalid events are skipped with a
    /// stderr diagnostic (the event log must stay append-only and corruption-tolerant).
    pub fn append_event(&mut self, event: SessionEvent) {
        if let Err(err) = self.validate_event(&event) {
            eprintln!("session_event: skipping invalid event {}: {}", event.event_id, err);
            return;
        }

        // Update caches before moving `event` into `self.events`, so we only
        // clone when the cache actually needs to retain the value.
        match &event.op {
            SessionEventOp::SetCompaction { .. } => {
                self.compaction_event_index = Some(event.clone());
            }
            SessionEventOp::ClearAll => {
                // Any cached compaction state refers to messages that no longer
                // exist after a clear; drop it so current_compaction() reflects
                // the cleared transcript.
                self.compaction_event_index = None;
            }
            _ => {}
        }
        self.events.push(event);
    }

    /// Append without validation — used for rehydration from trusted legacy vectors.
    pub(crate) fn push_event(&mut self, event: SessionEvent) {
        // Update caches before moving `event` into `self.events`, so we only
        // clone when the cache actually needs to retain the value.
        match &event.op {
            SessionEventOp::SetCompaction { .. } => {
                self.compaction_event_index = Some(event.clone());
            }
            SessionEventOp::ClearAll => {
                self.compaction_event_index = None;
            }
            _ => {}
        }
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
                    if *index < messages.len() {
                        messages.insert(*index, message.clone());
                    }
                }
                SessionEventOp::ReplaceMessages { start_index, end_index, messages: replace_with, .. } => {
                    // Clamp so the splice is always valid. `start_index` may
                    // equal the current length (a replacement issued after the
                    // transcript was cleared/truncated to empty must append
                    // rather than be silently dropped), and `end_index` may be
                    // usize::MAX for a full replacement.
                    let start = (*start_index).min(messages.len());
                    let end = (*end_index).min(messages.len());
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
        match &self.compaction_event_index {
            Some(event) => {
                if let SessionEventOp::SetCompaction { compaction } = &event.op {
                    Some(compaction.clone())
                } else {
                    None
                }
            }
            None => {
                // Fallback: cache may be empty after deserialization (serde(skip)).
                // Scan events in reverse to find the most recent SetCompaction.
                // If a ClearAll appears after that SetCompaction, the compaction
                // is considered cleared and we return None.
                let mut found_compaction = None;
                for event in self.events.iter().rev() {
                    match &event.op {
                        SessionEventOp::SetCompaction { compaction } => {
                            found_compaction = Some(compaction.clone());
                            break;
                        }
                        SessionEventOp::ClearAll => {
                            // ClearAll after the latest SetCompaction clears it
                            return None;
                        }
                        _ => {}
                    }
                }
                found_compaction
            }
        }
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
                fork.append_event(event.clone());
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
    
    /// Validate an event before adding to the map
    ///
    /// # Arguments
    /// * `event` - The event to validate
    ///
    /// # Returns
    /// Ok(()) if event is valid, Err(SessionEventError) otherwise
    fn validate_event(&self, event: &SessionEvent) -> Result<(), SessionEventError> {
        // Validate event_id
        if event.event_id.is_empty() {
            return Err(SessionEventError::InvalidEventId {
                event_id: event.event_id.clone()
            });
        }
        
        // Validate timestamp (not too far in future or past)
        let now = chrono::Utc::now();
        let timestamp_diff = (event.timestamp - now).num_seconds().abs();
        if timestamp_diff > 86400 * 365 { // Within 1 year
            return Err(SessionEventError::InvalidTimestamp {
                timestamp: event.timestamp
            });
        }
        
        // Validate operation based on event type
        match &event.op {
            SessionEventOp::AppendMessage { message, .. } => {
                self.validate_message(message, &event.event_id)?;
            }
            SessionEventOp::InsertMessage { message, .. } => {
                self.validate_message(message, &event.event_id)?;
            }
            SessionEventOp::SetCompaction { compaction } => {
                self.validate_compaction(compaction)?;
            }
            SessionEventOp::MemoryInjection { memory_injection } => {
                self.validate_memory_injection(memory_injection)?;
            }
            SessionEventOp::ReplayEvent { replay_event } => {
                self.validate_replay_event(replay_event)?;
            }
            _ => {} // Other operations don't need additional validation
        }
        
        Ok(())
    }
    
    /// Validate a message content
    fn validate_message(&self, message: &StoredMessage, message_id: &str) -> Result<(), SessionEventError> {
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
    fn validate_compaction(&self, compaction: &StoredCompactionState) -> Result<(), SessionEventError> {
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
    fn validate_memory_injection(&self, injection: &StoredMemoryInjection) -> Result<(), SessionEventError> {
        if injection.content.is_empty() {
            return Err(SessionEventError::InvalidMemoryInjection {
                reason: "Memory injection content cannot be empty".to_string()
            });
        }
        
        // Additional validation can be added here
        Ok(())
    }
    
    /// Validate replay event
    fn validate_replay_event(&self, replay_event: &StoredReplayEvent) -> Result<(), SessionEventError> {
        if replay_event.timestamp > chrono::Utc::now() {
            return Err(SessionEventError::InvalidTimestamp {
                timestamp: replay_event.timestamp
            });
        }
        
        Ok(())
    }
}

// Re-export types
pub use SessionEventOp as EventOp;
pub use SessionEvent as Event;
