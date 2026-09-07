//! Branded identity types for the event-sourced session log (takeaway #12).
//!
//! deepseek-harness brands `SessionId`, `ToolCallId`, `JobId`, `CompactionId`
//! as structurally-distinct strings so a `ToolCallId` can never be passed
//! where a `SessionId` is expected, catching whole classes of ID-mismatch
//! bugs at compile time rather than at log inspection time.
//!
//! jcode's session event log carries several identity-like `String` fields
//! that are *interchangeable* at the compile level only by accident of being
//! the same Rust type:
//!
//! - `SessionEvent.event_id` and `SessionEvent.parent_id` identify *events*;
//! - `AppendMessage.message_id` identifies *messages*;
//! - `CompactionStart.compaction_id` / `CompactionEnd` identifies *compaction
//!   brackets*.
//!
//! Folding these into distinct branded wrappers means the compiler rejects
//! passing a `MessageId` where an `EventId` is expected, even though both are
//! `String` under the hood. Each wrapper is `#[repr(transparent)]` over a
//! `String` with **`#[serde(transparent)]`**: it serializes/deserializes as
//! the bare string, so the wire format is byte-for-byte identical to the
//! previous `String` fields and persisted data round-trips unchanged.
//!
//! The extension rule matches the plan's intent without sprawling across
//! every `String` in the log: brand the *structural* identities the log uses
//! to correlate events, messages, and compaction brackets.

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
/// - has `Self::new(prefix)` generating a fresh id via `crate::id::new_id`,
///   and `Self::from_static(prefix, literal)` for deterministic test ids.
/// - exposes `Self::as_str()`/`Self::is_empty()` as the *only* read access; it
///   implements no `Deref`/`AsRef`/`Into<String>`, so a branded id cannot be
///   silently treated as (or round-tripped through) a generic string.
macro_rules! branded_id {
    (
        $(#[doc = $doc:literal])*
        $name:ident,
        $impl_doc:literal
    ) => {
        $(#[doc = $doc])*
        #[doc = $impl_doc]
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub(crate) String);

        impl $name {
            /// Generate a fresh id with the given `prefix` (e.g. `"event"`).
            pub fn new(prefix: &str) -> Self {
                Self(crate::id::new_id(prefix))
            }

            /// Build a deterministic id from a prefix + literal marker.
            ///
            /// Intended for tests and small fixed identities (e.g.
            /// `EventId::from_static("event", "rehydrate_0")`); production ids
            /// should prefer [`Self::new`] which includes a timestamp+random
            /// tail.
            pub fn from_static(prefix: &str, literal: &str) -> Self {
                Self(format!("{prefix}_{literal}"))
            }

            /// The underlying string.
            ///
            /// This is the *only* way to extract the raw string from a branded
            /// id. The type deliberately implements no `Deref`/`AsRef`/`Into<String>`
            /// so a branded id cannot be silently treated as a generic string
            /// (or round-tripped through `String`) without an explicit call.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branded_ids_are_distinct_types_but_same_layout() {
        let ev = EventId::from("e1".to_string());
        let msg = MessageId::from("m1");
        let comp = CompactionId::new("compaction");

        // PartialEq is only defined across the *same* type — the following
        // cross-type comparisons are compile errors, which is the point of
        // branding. Same-type equality works as expected.
        assert_eq!(EventId::from("e1"), ev);
        assert!(EventId::from("e2") != ev);
        assert_eq!(MessageId::from("m1"), msg);
        assert!(!comp.is_empty());
        assert!(!ev.is_empty());
    }

    #[test]
    fn serde_round_trips_as_bare_string() {
        let ev = EventId::from("event_1".to_string());
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, "\"event_1\"");
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn as_str_is_the_only_string_access() {
        let ev = EventId::from("event_x");
        assert_eq!(ev.as_str(), "event_x");
        // Display is available for formatting/logging but is NOT a conversion
        // back to `String` the caller can type-check against the id type.
        assert_eq!(ev.to_string(), "event_x");
        // The type deliberately provides no `Deref`/`AsRef<str>`/`Into<String>`,
        // so there is no way to treat the id as a bare string except `as_str`.
        // (This is enforced at compile time by the absence of those impls.)
    }

    #[test]
    fn from_static_is_deterministic() {
        let a = EventId::from_static("event", "rehydrate_0");
        let b = EventId::from_static("event", "rehydrate_0");
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "event_rehydrate_0");
    }
}
