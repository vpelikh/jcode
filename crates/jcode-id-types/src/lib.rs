//! Branded identity types shared across jcode crates (deepseek-harness #12).
//!
//! deepseek-harness brands `SessionId`, `ToolCallId`, `JobId`, `CompactionId`
//! as structurally-distinct strings so a `ToolCallId` can never be passed
//! where a `SessionId` is expected, catching whole classes of ID-mismatch
//! bugs at compile time rather than at log inspection time.
//!
//! This crate is the single canonical home for the `branded_id!` macro and the
//! branded identity wrappers it produces. It is a deliberately minimal leaf
//! crate: it depends only on `serde`, and the identity-bearing `-types` crates
//! (`jcode-message-types`, `jcode-protocol`, `jcode-background-types`,
//! `jcode-batch-types`, `jcode-session-types`, `jcode-harness-api`, ...) can
//! depend on it without creating a cycle, because none of them currently
//! depends on `jcode-base` (several are depended on *by* `jcode-base`).
//!
//! The branded identities:
//!
//! - `SessionId` backs the session's own identity (`Session.id`, wire structs);
//! - `ToolCallId` backs tool invocations (`ToolCall.id`, stream `tool_use_id`);
//! - `JobId` backs long-running background jobs (`DebugJob.id`);
//! - `EventId` backs `SessionEvent.event_id` / `parent_id`;
//! - `MessageId` backs `AppendMessage.message_id`;
//! - `CompactionId` backs `CompactionStart.compaction_id` / `CompactionEnd`.
//!
//! Each wrapper is `#[repr(transparent)]` over a `String` with
//! **`#[serde(transparent)]`**: it serializes/deserializes as the bare string,
//! so the on-disk / on-wire format is byte-for-byte identical to the previous
//! `String` fields and persisted data round-trips unchanged.
//!
//! The branded types are structurally distinct, so cross-type assignment is a
//! compile error (compile-time checked by the doctest below):
//!
//! ```compile_fail
//! use jcode_id_types::{SessionId, ToolCallId};
//!
//! let tool_call_id = ToolCallId::from("call_1");
//! let session_id: SessionId = tool_call_id; // mismatched types
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

/// Macro for a `#[repr(transparent)]` branded id over `String`.
///
/// The generated type:
/// - owns one `String` (`.0`), `#[repr(transparent)]` so it is layout-free;
/// - serializes/deserializes as the bare string (`#[serde(transparent)]`), so
///   the on-disk / on-wire format is identical to the previous raw `String`;
/// - implements `Clone`, `Debug`, `PartialEq`, `Eq`, `Hash`, `PartialOrd`,
///   `Ord`, `Display`, `From<String>`, and `From<&str>`;
/// - constructs via `From<String>`/`From<&str>` (callers use `.into()`, e.g.
///   `crate::id::new_id("event").into()`);
/// - exposes `Self::as_str()`/`Self::is_empty()` for explicit borrowed access;
///   it implements no `Deref`/`AsRef`/`Into<String>`, so a branded id cannot be
///   *silently* dereferenced to a generic string. `Display` is provided only for
///   formatting/error text; extracting the string still requires an explicit
///   `{}`/`.to_string()` call, never an implicit coercion at a `&str`/`String`
///   site.
///
/// The macro is `#[macro_export]` with a crate-relative path so it is usable
/// from any crate: `jcode_id_types::branded_id!`.
#[macro_export]
macro_rules! branded_id {
    (
        $(#[doc = $doc:literal])*
        $name:ident,
        $impl_doc:literal
    ) => {
        $(#[doc = $doc])*
        #[doc = $impl_doc]
        #[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub(crate) String);

        impl $name {
            /// The underlying string, as a borrowed `&str`.
            ///
            /// This is the explicit way to read the raw string from a branded id.
            /// The type implements no `Deref`/`AsRef`/`Into<String>`, so a branded
            /// id is never *implicitly* coerced to a generic string; `Display`
            /// (`.to_string()`) exists only for formatting/error text.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Whether the underlying string is empty.
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_fmt(format_args!("{}({:?})", stringify!($name), self.0))
            }
        }
    };
}

branded_id!(
    /// Globally-unique identifier for a single event in the session event log.
    ///
    /// Backs `SessionEvent.event_id` (the event's own id) and
    /// `SessionEvent.parent_id` (the id of the event this one derives from, for
    /// merge-extensibility). Branded so an event id can never be passed where a
    /// message or compaction id is expected.
    EventId,
    "A branded id identifying a session-log event."
);

branded_id!(
    /// Identifier for a stored message within a session transcript.
    ///
    /// Backs the `message_id` of `SessionEventOp::AppendMessage`. Branded so a
    /// message id can never be passed where an event or compaction id is
    /// expected.
    MessageId,
    "A branded id identifying a session message."
);

branded_id!(
    /// Identifier for a compaction bracket (an open/closed log marker).
    ///
    /// Backs `SessionEventOp::CompactionStart.compaction_id`, tying a crashed
    /// bracket back to the span being summarized (takeaway #5).
    CompactionId,
    "A branded id identifying a compaction bracket in the event log."
);

branded_id!(
    /// Globally-unique identifier for a session.
    ///
    /// Backs `Session.id`, `ServerEvent::SessionId`, `StreamEvent::SessionId`,
    /// and the `session_id` of wire/overview structs. Branded so a session id can
    /// never be passed where a tool-call, job, event, message, or compaction id
    /// is expected.
    SessionId,
    "A branded id identifying a session."
);

branded_id!(
    /// Globally-unique identifier for a single tool invocation.
    ///
    /// Backs `ToolCall.id`, the stream `ToolUseStart.id` / `ToolResult.tool_use_id`,
    /// and `Agent.tool_call_ids`. Branded so a tool-call id can never be passed
    /// where a session, job, event, message, or compaction id is expected.
    ToolCallId,
    "A branded id identifying a tool call."
);

branded_id!(
    /// Globally-unique identifier for a long-running background/debug job.
    ///
    /// Backs `DebugJob.id` and the `job_id` returned to debug commands. Branded
    /// so a job id can never be passed where a session, tool-call, event, message,
    /// or compaction id is expected.
    JobId,
    "A branded id identifying a background job."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branded_ids_are_distinct_types_but_same_layout() {
        let ev = EventId::from("e1".to_string());
        let msg = MessageId::from("m1");
        let comp = CompactionId::from("comp_1");
        let sess = SessionId::from("sess_1");
        let tool = ToolCallId::from("tool_1");
        let job = JobId::from("job_1");

        // PartialEq is only defined across the *same* type — the following
        // cross-type comparisons are compile errors, which is the point of
        // branding. Same-type equality works as expected.
        assert_eq!(EventId::from("e1"), ev);
        assert!(EventId::from("e2") != ev);
        assert_eq!(MessageId::from("m1"), msg);
        assert!(!comp.is_empty());
        assert!(!ev.is_empty());
        assert_eq!(SessionId::from("sess_1"), sess);
        assert_eq!(ToolCallId::from("tool_1"), tool);
        assert_eq!(JobId::from("job_1"), job);
        assert_eq!(sess.as_str(), "sess_1");
        assert_eq!(tool.as_str(), "tool_1");
        assert_eq!(job.as_str(), "job_1");
    }

    #[test]
    fn serde_round_trips_as_bare_string() {
        let ev = EventId::from("event_1".to_string());
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, "\"event_1\"");
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);

        let sess = SessionId::from("sess_a");
        let j = serde_json::to_string(&sess).unwrap();
        assert_eq!(j, "\"sess_a\"");
        let back_sess: SessionId = serde_json::from_str(&j).unwrap();
        assert_eq!(back_sess, sess);

        let tool = ToolCallId::from("tool_x");
        let j = serde_json::to_string(&tool).unwrap();
        assert_eq!(j, "\"tool_x\"");
        let back_tool: ToolCallId = serde_json::from_str(&j).unwrap();
        assert_eq!(back_tool, tool);

        let job = JobId::from("job_y");
        let j = serde_json::to_string(&job).unwrap();
        assert_eq!(j, "\"job_y\"");
        let back_job: JobId = serde_json::from_str(&j).unwrap();
        assert_eq!(back_job, job);
    }

    #[test]
    fn as_str_is_the_explicit_string_access() {
        let ev = EventId::from("event_x");
        assert_eq!(ev.as_str(), "event_x");
        // Display is available for formatting but is an explicit conversion
        // (a `{}`/`.to_string()` call), not an implicit coercion to `&str`/`String`.
        assert_eq!(ev.to_string(), "event_x");
        // The type provides no `Deref`/`AsRef<str>`/`Into<String>`, so it can
        // never be *implicitly* used where a `&str` or `String` is expected.
    }

    #[test]
    fn new_branded_ids_round_trip_wire_format() {
        // Session ids are ordinary memorable strings in jcode (e.g. "fox-oak").
        let sess = SessionId::from("lively-fox".to_string());
        assert_eq!(serde_json::to_string(&sess).unwrap(), "\"lively-fox\"");
        let back: SessionId = serde_json::from_str("\"lively-fox\"").unwrap();
        assert_eq!(back, sess);

        // Tool call ids are provider-supplied opaque strings.
        let tool = ToolCallId::from("call_abc123".to_string());
        assert_eq!(serde_json::to_string(&tool).unwrap(), "\"call_abc123\"");
        let back: ToolCallId = serde_json::from_str("\"call_abc123\"").unwrap();
        assert_eq!(back, tool);

        let job = JobId::from("job_vivid".to_string());
        assert_eq!(serde_json::to_string(&job).unwrap(), "\"job_vivid\"");
        let back: JobId = serde_json::from_str("\"job_vivid\"").unwrap();
        assert_eq!(back, job);
    }
}
