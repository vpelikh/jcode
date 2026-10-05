# Why test runs got slow: unbounded concurrent builds and a gate that no-oped on macOS

## Symptom

`cargo test` runs across jcode worktrees became dramatically slower. The wrapper's
own action log (`~/.jcode/logs/rust-actions.jsonl`) shows a step change:

| date      | test runs | median | p90     | total test time |
|-----------|-----------|--------|---------|-----------------|
| 2026-09-30| 167       | 12.1 s | 46.8 s  | 67 min          |
| 2026-10-01| 145       | 12.7 s | 60.5 s  | 82 min          |
| 2026-10-03| 1416      | 45.6 s | 244.9 s | **2330 min**    |
| 2026-10-04| 877       | 46.4 s | 173.1 s | **1218 min**    |

The median roughly 4x'd and daily total test time ballooned ~30x. Per-package
medians confirm it is a real regression, not a change in which commands ran:
jcode-app-core 5.6x, jcode-base 3.4x, jcode-tui 4.3x, jcode-protocol 12.6x.

## Root cause

**1. Many worktrees built at once.** Distinct repositories issuing cargo actions
rose from 1-3 (Sep 30/Oct 1, max 2 concurrent) to 7 on Oct 3 and 13 on Oct 4,
with a measured max of 8 concurrent compile actions:

| date       | cargo actions | distinct repos | max concurrent |
|------------|---------------|----------------|----------------|
| 2026-09-30 | 225           | 1              | 2              |
| 2026-10-01 | 190           | 3              | 2              |
| 2026-10-03 | 1809          | 7              | **8**          |
| 2026-10-04 | 1147          | 13             | **7**          |

This concurrency is the upstream cause of the Oct 3 regression, before any
shared target dir existed.

**2. The gate that was supposed to bound it silently disabled itself.**
`scripts/dev_cargo.sh` tried to serialize compile-capable cargo actions across
worktrees with the `flock` CLI. **macOS ships no `flock`**, so:

```
dev_cargo: flock is unavailable; running without the host-wide Cargo gate
```

The gate no-oped. `gate_wait_ms` is 0 on all 17,725 records, which is the proof it
never ran.

**3. A shared target directory compounds it (Oct 4 onward).** `~/.zshrc` exports
`JCODE_SHARED_TARGET_DIR`, so every worktree's cargo build then pointed at one
target dir. Cargo takes an exclusive lock on a build directory, so those builds
serialize themselves and queue on `Blocking waiting for file lock on build
directory` (seen in 25 scratch logs). The dir was created **Oct 4 06:39**, so it
explains the tail, not the Oct 3 step.

## Why the gate is a semaphore, not a mutex

The original gate was a mutual-exclusion lock: one build at a time. That is the
wrong primitive here, and two independent sources say so:

- Cargo already takes an **exclusive lock on a shared build directory**; two
  builds sharing `CARGO_TARGET_DIR` do not run in parallel. So a mutex gate is
  redundant with Cargo when targets are shared.
- A mutex also serializes build *sets* that use **separate** target dirs, which
  Cargo would happily run in parallel; that would leave the expensive test index
  build of one worktree waiting behind a trivial check of another. Sharing one
  target dir across worktrees was explicitly rejected in the Kronn design note
  ("Sharing serialises exactly what we run in parallel") and documented as unsafe
  in rust-lang/cargo#16804.

The gate is therefore a **counting semaphore** whose capacity bounds total
compiler (rustc) concurrency:

- **Shared target dir** (`JCODE_SHARED_TARGET_DIR` set): capacity 1, because
  Cargo serializes those anyway. The gate only makes the wait visible and stops a
  crowd from hitting the target-dir lock.
- **Per-worktree target dir**: capacity so that
  `capacity * CARGO_BUILD_JOBS` stays near a target share of the cores, also
  bounded by memory. Default target is 1x cores (no over-allocation); change the
  factor with `JCODE_CARGO_GATE_RUSTC_OVERSUB`. This bounds total rustc
  processes, not just concurrent builds: with 4 cores and 4 jobs per build the
  capacity is 1, so at most 4 rustc run, not 8.

The capacity reuses the same memory/CPU sizing as `select_build_jobs`. Override
with `JCODE_CARGO_GATE_SLOTS`; disable with `JCODE_CARGO_GATE=off`; nested wrapper
calls inherit.

Capacity is enforced uniformly, on every platform, with atomically-created slot
directories (`mkdir`), each holding the holder PID in an `owner` file; a slot
whose owner is dead is reclaimed. The original `flock`-based gate is gone
entirely, which removes the macOS/Linux split that let the gate no-op. Because the
wrapper process holds a slot for the whole build, a `bash`-launched `cargo` that
extracts the toolchain does not slip the gate: the wrapper waits for cargo to exit
before its `EXIT` trap releases the slot.

Several details matter for correctness of the reclaim:

- A slot is published by `mkdir` before its `owner` file is written, so a slot
  with no owner is only reclaimed once it is older than a grace period
  (`JCODE_CARGO_GATE_STALE_GRACE`, default 10 s). Reclaiming a fresh owner-less
  slot would let two builds share one slot and exceed the bound.
- Release removes a slot only if its owner file is ours, so one process can never
  delete a slot another process now holds.
- A slot whose owner PID is dead is reclaimed, so a crashed build cannot consume
  a slot forever. The owner's process start time is stored too (`ps -o lstart=`),
  so a leaked slot whose pid was later recycled by an unrelated live process is
  still detected as stale; where the start time cannot be read it falls back to
  pid-only liveness.
- While a gated command runs, the wrapper defers INT/TERM until cargo exits, so a
  signal to the wrapper cannot free the slot while the compiler is still running
  (which would let another build start and exceed the bound). A SIGKILL cannot be
  deferred; it leaves a stale slot that the next build reclaims. A deferred TERM
  is reported as the conventional 128+15 exit status once the slot is released.

## Why not `cargo-worktree`?

`cargo-worktree` (crates.io, 2026) is a thin Cargo wrapper that sets
`CARGO_TARGET_DIR` to `target/git/<branch-or-commit>` and execs cargo, giving each
worktree its own target dir. It is worth taking seriously and does not fit here:

- **It does not bound concurrency.** The regression was N worktrees each running a
  build with many rustc processes at once (8 builds, load ~95 on 12 cores). Giving
  each worktree its own target dir removes Cargo's build-dir lock and does nothing
  to cap total parallelism; it can even increase contention on the shared
  `~/.cargo` registry lock. The semaphore is what actually bounds the machine.
- **It multiplies disk.** Each worktree then compiles and stores the crate graph
  itself. With `JCODE_SHARED_TARGET_DIR` unset, this host is at ~13 GiB free (97%
  used) and ~104 GiB of shared target; 8 worktrees at 7-23 GiB each does not fit.
  `cargo-worktree`'s own README notes it shares nothing between worktrees.

`cargo-worktree` would only be attractive if disk were plentiful and the goal were
per-worktree isolation, not bounded parallelism. It and the semaphore are also
composable: if disk ever allows isolated targets, the semaphore (which self-limits
to capacity 1 only for a shared target dir) still bounds the machine.

## Validation

- `scripts/test_dev_cargo_gate.sh` (wired into `scripts/check_guardrails.sh` and
  CI) covers: only compile-capable actions gated; capacity 1 admits one and
  blocks others; capacity 2 admits two and blocks the third; the third runs after
  a slot frees; stale slots are reclaimed; release removes the slot;
  `JCODE_CARGO_GATE=off` disables; nested calls inherit without taking a slot.
- End-to-end, two concurrent `dev_cargo.sh check` with **isolated** target dirs
  take different slots (0/2 and 1/2) with `gate_wait=0` and run in parallel.
- End-to-end, two concurrent runs against the **shared** target dir serialize
  (capacity 1), with the second recording a nonzero `gate_wait`.
- `scripts/test_dev_cargo_jobs.sh` (existing) still passes.
- `scripts/test_dev_cargo_gate_integration.sh` drives the real wrapper with a
  fake slow `cargo` and checks slot release on success, on failure (exit code
  propagated), that SIGTERM does not free the slot while cargo runs (released only
  after cargo exits), and stale reclaim after SIGKILL.
- `scripts/test_dev_cargo_gate_acceptance.sh` is the acceptance check: it launches
  N real concurrent builds through the wrapper with a fake slow `cargo` and
  samples concurrent cargo processes. Observed maximum equals capacity exactly
  (capacity 1/2/3 under 4-5 launches), never N. This is the concrete "it is
  better" observation: the bound that was absent (8 concurrent builds) now holds.

### Aggregate wall time

Wall-clock A/B on the shared target dir was inconclusive, and it is worth saying
so plainly rather than quoting a favorable number:

| measurement                                   | result |
|-----------------------------------------------|--------|
| 6 concurrent checks, one after the other       | unbounded 811 s vs bounded 674 s |
| interleaved round 1 (off then on, close in time)| unbounded 635 s vs bounded 939 s |

The sequential run suggested the gate was faster; the interleaved run, where the
two arms are close enough in time that host load barely changed between them,
reversed it. That reversal is the honest picture: on the **shared target dir**
cargo's own build-dir lock already serializes the builds, so the gate adds a
second wait on top, and aggregate wall time is dominated by cargo's lock plus
general host load, not by the gate. The gate does not make shared-target builds
faster; at best it is neutral.

What the gate does guarantee, verified by construction and by the tests above, is
the bound: no more than `capacity` compile-capable actions run concurrently. That
is the property whose absence caused the regression (8 concurrent builds, load 95
on 12 cores). The gate is a safety bound against unbounded concurrency; it is not
a throughput optimization for the shared-target case, and this section exists so
nobody later mistakes one for the other.

## Operational note

The gate only takes effect once it is landed in the shared checkout and the
daemon/binary that runs agent builds is repointed; until then unpatched
processes keep bypassing it (31 logs still show `flock is unavailable` while
patched ones wait). Long waits under a shared target dir mean the machine is
genuinely busy with another build; that is now visible as `gate_wait_ms` rather
than hidden.

## Should we stop sharing one target dir across worktrees?

Sharing serializes builds that could run in parallel, so the tempting follow-up is
sitting targets. Measured on this host (2026-10-04, numbers are approximate and
move while builds are active), that is not affordable:

| what                              | size   |
|-----------------------------------|--------|
| worktrees in play                 | 8      |
| free disk                         | ~36-39 GiB (91-92% used) |
| shared target dir                 | ~96 GiB (60 GiB debug + 36 GiB selfdev) |
| one old worktree's own target     | ~22 GiB |
| another old worktree's own target | ~5-7 GiB |

The two per-worktree targets already on disk are leftovers from before the shared
dir existed, and they alone duplicate ~27 GiB. Extrapolating, 8 worktrees at
7-23 GiB each is 56-184 GiB of target, against ~36 GiB free: a full switch to
per-worktree targets does not fit, and it would multiply the crate graph once per
worktree (bounded only by a lifecycle cleanup that does not exist yet). So the
current choice -- one shared target dir plus this semaphore -- is the correct
compromise under the disk constraint: sharing caps disk, and the semaphore stops
the shared dir from turning concurrent builds into a thundering herd. The
parallelism loss is real but it is the cheaper of the two problems here.

Revisit only if either the disk budget grows enough to hold N worktree targets,
or a worktree-target lifecycle cleanup is added (retire a `target/` when that
worktree's work is durably finished).

## Known trade-offs

What the gate costs, and which parts were removable:

- **Bounds builds, not rustc directly.** The capacity now also accounts for
  `CARGO_BUILD_JOBS`, so `capacity * jobs` tracks the core count; it is still a
  build-level bound, not a per-rustc cap, by design (one build must be free to use
  all cores).
- **Redundant with Cargo on a shared target dir.** Cargo's build-dir lock already
  serializes those builds; the gate there only makes the wait visible. This is
  inherent to the shared-dir setup, not to the gate; it disappears if per-worktree
  targets become affordable. Measured as roughly neutral-to-slightly-negative on
  throughput for the shared-dir case, so the gate's value there is the bound, not
  speed.
- **Per-user by default.** The gate dir is `~/.jcode/run/jcode-cargo-gate`, so
  different OS users do not coordinate. A host owner can point everyone at one
  directory with `JCODE_CARGO_GATE_DIR_HOST` (slots are mkdir-atomic and
  owner-fenced, so shared use is safe).
- **~0.25 s acquisition latency** for a queued build (the slot poll interval).
- **Signal deferral.** A TERM/INT to the wrapper is delayed until the gated cargo
  exits, then reported as 128+signum. Agent cancellation is unaffected (it uses
  SIGKILL).
- **`--print-setup` key renamed** `cargo_gate_path` -> `cargo_gate_dir`; no
  in-repo consumer parses it, but an external one would need updating.
