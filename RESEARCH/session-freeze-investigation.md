# Investigation: many jcode sessions freezing

**Date:** 2026-09-01
**Branch (worktree):** `research/session-freeze`

## Symptom
Many concurrently-open jcode sessions appeared to freeze (unresponsive TUI). All at
once.

## Data gathered
- 32 GB Mac, 12 logical cores (8P/4E).
- Load average *spiked*: 13–22 at start, reached **33+** during sampling. Well above 12.
- Memory: free pages dropped to ~119k (= ~1.9 GB), with heavy compression activity
  (199M compressions, 140M decompressions since boot) while ~26 jcode-family
  processes were resident.
- Multiple **concurrent `cargo`/`rustc`/`clippy` builds** were running at the same
  time (8 rust/cargo procs seen), plus ~10 `self-dev --resume` resident agent
  sessions, plus 3 interactive `jcode` TUIs, plus the shared-server daemon
  (`jcode serve`).
- The shared-server daemon (`jcode.sock`) held **20+ live client connections**,
  confirming many sessions multiplex through one daemon.
- `sample` of a "frozen" interactive TUI (53074) showed it spinning in
  `crossterm poll_internal -> parking_lot RawMutex::lock_slow -> SpinWait::spin`
  briefly, then settling into clean `__psynch_cvwait` waits. This is the classic
  signature of **transient CPU starvation**, not a deadlock.

## Root cause
**System-level CPU + memory oversubscription.** ~26 jcode-family processes
(10+ resident self-dev agents + multiple interactive TUIs + shared daemon) running
simultaneously with several concurrent Cargo builds vastly exceed 12 cores and put
RAM under compression pressure. Sessions don't deadlock; they stall on scheduler
latency and memory churn, which reads as "freezing."

The shared architecture is relevant but not the bug: all sessions route through one
daemon, and the per-connection accept path (`client_count` RwLock, spawn-per-connection)
is short-lived and does not serialize clients. The daemon sampled as idle in `kevent`.

## Evidence samples
- `runtime.rs` accept loop: `increment_client_count`/`spawn_client_task` per
  connection, no long global lock held.
- `sample 53074`: `__psynch_cvwait` ~7500 samples (clean), occasional crossterm
  mutex spin during load spikes.
- `sample 97630` (daemon): `__psynch_cvwait` 216, `kevent` 15 (idle).

## Recommendations (mitigation)
1. **Stop concurrent cargo builds** while many sessions are open (biggest lever:
   each rustc/clippy job pegs a core).
2. **Close or suspend idle self-dev resident sessions** (`self-dev` agents stay
   resident and share the daemon). Only keep actively-used ones.
3. Consider bounding concurrent compile jobs (`cargo build -j2`, `CARGO_BUILD_JOBS`)
   during multi-session development.
4. Optionally cap how many self-dev agents are left resident; each holds memory and
   wakes on shared daemon events.
5. If this recurs frequently, add a lightweight instrumentation/alert for client
   count + load in the daemon to catch oversubscription before it becomes "all
   frozen."