use chrono::{DateTime, Utc};
use jcode_session_types::{StoredCompactionState, StoredMemoryInjection, StoredMessage};
use crate::session::model::StoredReplayEvent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// Index of events by type for efficient querying
    #[serde(skip)]
    message_index: HashMap<usize, SessionEvent>,
    #[serde(skip)]
    compaction_event_index: Option<SessionEvent>,
}

impl SessionEventMap {
    /// Append a new event to the map
    pub fn append_event(&mut self, event: SessionEvent) {
        let event_index = self.events.len();
        self.events.push(event.clone());
        
        // Update indices
        match &event.op {
            SessionEventOp::AppendMessage { message_id, .. } => {
                // Parse message_id to get index if available, or store by event_id
                if let Ok(index) = message_id.parse::<usize>() {
                    self.message_index.insert(index, event.clone());
                }
            }
            SessionEventOp::SetCompaction { .. } => {
                self.compaction_event_index = Some(event);
            }
            _ => {}
        }
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
                    if *index <= messages.len() {
                        messages.insert(*index, message.clone());
                    }
                }
                SessionEventOp::ReplaceMessages { start_index, end_index, messages: replace_with, .. } => {
                    let start = *start_index;
                    if start < messages.len() {
                        let end = (*end_index).min(messages.len());
                        messages.splice(start..end, replace_with.clone());
                    }
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
            None => None,
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
}

// Re-export types
pub use SessionEventOp as EventOp;
pub use SessionEvent as Event;
