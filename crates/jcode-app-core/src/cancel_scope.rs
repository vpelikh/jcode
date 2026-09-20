//! Shared cooperative-cancellation scope for blocking per-call work.
//!
//! A background task that cannot be cancelled mid-yield (for example a blocking
//! `spawn_blocking` scan, where dropping the async future around it only leaves
//! the pooled thread running) cooperates by checking a shared flag between its
//! iterations. [`CancelScope`] is that flag paired with a [`Drop`]-armed guard
//! that flips it when the owning future is dropped — the natural way for a
//! `tokio::time::timeout` or a caller that abandons a call to signal "stop when
//! you can" without an invasive per-call token threaded through every
//! constructor.
//!
//! Guard semantics are exactly-once: dropping a guard stores the boolean flag,
//! so a single canceller is enough and there is no torn or double set to reason
//! about.
//!
//! See `session_search` (the working consumer) and the
//! runtime task-scope test helpers for usage. As of the F8 Part A work this is
//! deliberately a *same-crate* helper: it has exactly one production consumer,
//! so it does not yet earn a leaf crate; when a second real consumer appears it
//! can be promoted (see the status doc).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A shared cancellation flag plus a [`Drop`]-armed guard.
///
/// The flag is readable from any thread via [`CancelScope::cancelled`]; a
/// [`CancelScope::guard`] arms it once when dropped, so a loop can bail at a
/// convenient boundary after a timer or an abandoned call drops the surrounding
/// future.
#[derive(Debug)]
pub struct CancelScope {
    flag: Arc<AtomicBool>,
}

impl CancelScope {
    /// A fresh scope with the flag cleared.
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns `true` once the scope has been cancelled (a guard dropped).
    pub fn cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Creates a guard that arms this scope when dropped. Arm it before any
    /// `await` that may be cancelled by the caller dropping the future.
    pub fn guard(&self) -> CancelGuard {
        CancelGuard {
            flag: Arc::clone(&self.flag),
        }
    }

    /// Makes a clone whose [`CancelGuard`]s observe the same flag. Used to hand
    /// the flag to a spawned task while keeping the guard in the caller.
    pub fn child(&self) -> Self {
        Self {
            flag: Arc::clone(&self.flag),
        }
    }
}

/// Sets the scope's flag on drop. Created via [`CancelScope::guard`]; when the
/// owning future is dropped (e.g. by `execute_with_deadline`'s timeout) this
/// guard's `Drop` marks the scope cancelled for the in-flight task. It is an
/// RAII guard, not a `Clone`able handle: you make one to own the cancellation
/// of one future.
pub struct CancelGuard {
    flag: Arc<AtomicBool>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_scope_is_not_cancelled() {
        let scope = CancelScope::new();
        assert!(!scope.cancelled());
    }

    #[test]
    fn dropping_guard_arms_the_shared_flag() {
        let scope = CancelScope::new();
        assert!(!scope.cancelled());
        {
            let _guard = scope.guard();
            assert!(!scope.cancelled(), "guard alive => not yet cancelled");
        }
        assert!(scope.cancelled(), "dropping the guard must arm the flag");
    }

    #[test]
    fn alias_shares_the_same_flag() {
        let scope = CancelScope::new();
        let alias = scope.child();
        assert!(!alias.cancelled());
        {
            let _guard = alias.guard();
        }
        assert!(
            scope.cancelled(),
            "a guard dropped on an alias must arm the shared flag"
        );
    }

    #[test]
    fn a_scope_is_shiftable_into_a_blocking_task() {
        let scope = CancelScope::new();
        let alias = scope.child();
        let worker = std::thread::spawn(move || {
            // Simulate a worker observing the flag between iterations.
            while !alias.cancelled() {
                std::thread::yield_now();
            }
        });
        drop(scope.guard());
        // Join so a cancellation that never fired (a regression) would block
        // forever and fail the test loudly instead of leaking a busy-waiting
        // thread past the test's end.
        worker
            .join()
            .expect("worker must observe the cancellation and exit");
        assert!(
            scope.cancelled(),
            "the scope must be cancelled after its guard drops"
        );
    }
}