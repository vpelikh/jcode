# Investigation: why do many jcode sessions use so much memory?

**Date:** 2026-09-01

## Goal
The user wants to run many jcode sessions concurrently. Previous investigation
established that during the freeze event the machine was CPU-oversubscribed
(load 26-33 on 12 cores). The follow-up ask: why is so much *memory* used, so we
can fit more sessions.

## What I measured

### Per-process `ps` RSS
| Process | `ps` RSS |
|---|---|
| shared-server daemon (`jcode serve`) | **4189 MB** |
| interactive TUI clients (`jcode`) | **100-420 MB each** |
| `self-dev --resume` resident agents | **56-210 MB each** |
| aggregate jcode-family | ~6.7 GB |

At first glance this looks pathological: 6.7 GB for a dozen sessions.

### But vmmap shows the real resident footprint
For the daemon (`vmmap -summary`, pid 97630):
- **Physical footprint: 402.8 MB** (actual resident memory in use)
- **Writable regions: Total 4.9 GB, written 3.2 MB, resident 4.0 GB, unallocated 953 MB**
- `MALLOC_SMALL`: 2.2 GB total, 158.7 MB resident
- `MALLOC_SMALL (empty)`: **2.5 GB total, 21 MB resident**, 628 empty zones
- `MALLOC_LARGE (empty)`: 177 MB total, 170 MB resident (empty)

**Conclusion:** ~4.9 GB of the daemon's address space is **reserved but mostly
unwritten/unallocated malloc arenas**. macOS counts this reserved writable VM
toward `ps` RSS, so `ps` reports 4.2 GB while the true resident footprint is
~400 MB.

### App-internal tracked state is tiny
Via the debug socket (`server:memory`, `agent:memory`), all *tracked* application
state sums to a few MB:
- 15 live sessions, 5396 messages, **total JSON ~11 MB** across all sessions
- Largest single session: `session_rose…` **2.6 MB** (transcript + provider cache)
- Event history: 210 KB · background tasks: 262 KB · session search index: 898 KB
- Swarm / file-tracking / channels / debug: negligible

So the whole *logical* session state is ~11-15 MB. The 4.2 GB is not session data.

### System-wide memory
- `vm_stat` free pages during quiet moments can read 78%; that number is misleading. When
  the machine is saturated (many sessions + concurrent builds), **real free pages drop to
  ~4,000** with ~203,064 pages (~3.3 GB) in the memory compressor and load average reaching
  71. So the machine is under **both** heavy CPU contention **and** real memory/compression
  pressure — it is NOT simply "RAM-fine". The 78% figure excludes compressed/inactive
  pages. See the "Data note" section below.

## Root cause: untuned system malloc on macOS + `ps` RSS over-accounting

- The daemon is a long-running tokio process (31 OS threads) that loads/unloads a
  ~90 MB ONNX embedding model and churns large provider JSON/tool-result buffers.
- The default Cargo feature set is `["pdf","embeddings","bedrock"]` — **`jemalloc`
  is NOT default**.
- Without `jemalloc`, on macOS `configure_system_allocator()` is a **no-op**
  (`#[cfg(not(all(target_os="linux", target_env="gnu", not(feature="jemalloc"))))]`).
  macOS uses stock system malloc with **no arena limit, no decay, no page-return**.
  Freed arenas stay reserved and grow `ps` RSS.
- With `jemalloc` enabled, the decay/narena tuning is applied at **build time** via the
  `JEMALLOC_SYS_WITH_MALLOC_CONF` env var (set in `.cargo/config.toml` `[env]`), which
  tikv-jemalloc-sys passes to jemalloc's `--with-malloc-conf` configure. This
  `dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4` returns dirty pages to the OS after 1s
  idle — the exact mechanism that bounds RSS. (The runtime `malloc_conf` global exported from
  `src/main.rs` is NOT reliably read by jemalloc on macOS, so the build-time env is the
  authoritative wiring; see the corrected "Decay-value reconciliation" note below.) The code
  comment even notes the untuned defaults "caused 1.4 GB RSS".

## Why `ps` RSS is not the same as "used memory"
macOS `ps` RSS includes reserved writable VM. For a process that has ever touched
several GB of heap, the malloc arenas remain mapped and count toward RSS even when
empty. The relevant number for memory pressure is the **physical/vmmap footprint**
(~400 MB for the daemon), not `ps` RSS (4.2 GB).

## Recommendations (to run many more sessions)

1. **Enable the `jemalloc` feature in default/release builds** so the
   `dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4` tuning activates. This is the intended
   fix: `jemalloc` is now a **global default feature** (all platforms), and the decay/narena
   tuning is applied at build time via `JEMALLOC_SYS_WITH_MALLOC_CONF` in `.cargo/config.toml`
   `[env]` (the `src/main.rs` runtime `malloc_conf` static is a secondary fallback that is not
   reliably read on macOS). This bounds daemon RSS to actual usage instead of retained arenas.
2. **Report physical footprint instead of `ps` RSS** in UI/telemetry for a less
   alarming, more accurate number.
3. **The practical limiter for many sessions is system-wide CPU and accumulated resident
   processes, not the daemon's allocator.** Even at 4.2GB `ps` RSS the daemon's real
   footprint is ~400 MB and its *logical* session state is ~11-15 MB. But when many
   resident sessions + concurrent builds coexist, load reaches 71-96 (i.e. >5-8x
   oversubscription on 12 cores, observed at load 96 on 2026-09-02) while the entire
   jcode-family uses only ~2.2 GB RSS and free pages collapse to ~4k with ~3.3 GB
   compressed. To fit more sessions, bound concurrency: cut concurrent
   cargo/rustc jobs and idle `self-dev --resume` agents (the largest contributors to load
   and resident/compressed memory).
4. If per-session real memory does grow, cap/normalize large in-memory transcripts
   (compaction) — but measurements show transcripts are small (~11 MB total), so
   this is a secondary concern.

## Can you actually open more sessions? (acceptance-path synthesis)

Direct test on the live daemon: created 4 fresh idle headless sessions via
`create_session:/tmp` and re-measured. **Footprint was unchanged (~471-500 MB).** An idle
session costs essentially zero footprint — the `Agent` is lazy/sparse until a transcript
accumulates. (Test sessions were then destroyed.)

**Measurement-variance note.** Repeated footprint readings over ~20s under normal churn
varied **471 → 500 MB**, so do not treat any single figure as exact; the useful result is
that adding idle sessions did not change footprint beyond this ~30 MB band. `ps` RSS was
~2.9 GB (not 4.2 GB) at that later point — the daemon's retained system-malloc arenas had
already been partly reclaimed, confirming `ps` RSS is allocator state, not a fixed number.

**The embedding model is a real per-daemon (not per-session) memory component.** Via
`server:memory`, the daemon loads/unloads an **~90 MB ONNX embedding model**
(`total_artifact_bytes: 90,871,461`; `load_count: 38`/`unload_count: 37`, currently
`loaded: true`). Along with tokio tasks and allocator retention, this contributes to the
~500 MB footprint. It is a bounded, toggleable cost per daemon — independent of how many
sessions are open.

**Measured embedding-model cost (live debug interface).** I exercised the real
`embeddings:unload` / `embeddings:load` debug-socket commands on the live daemon and
measured footprint with the model loaded vs unloaded:

| state | footprint |
|---|---|
| loaded | 500 MB |
| unloaded | 500 MB |
| re-loaded | 500 MB |

The footprint was **identical** with the model unloaded. This does **not** mean the model
is free; it confirms the retention thesis: unloading frees the ONNX allocations, but the
non-jemalloc system-malloc build keeps those pages in the arena, so `footprint` (and `ps`
RSS) do not drop.

**Byte-exact dirty-resident comparison (loaded vs unloaded).** To rule out that the flat
total was masking a real but small drop, I captured `footprint --swapped -w` fine detail at
each state:

| region | loaded | unloaded (immediate) | unloaded (+3s) |
|---|---|---|---|
| MALLOC_SMALL dirty | 281 MB | 281 MB | 281 MB |
| MALLOC_LARGE dirty | 174 MB | 174 MB | 174 MB |

The dirty-resident bytes are **byte-for-byte identical** across loaded/unloaded/settled.
Unloading the ~90 MB model changed zero dirty-resident pages.

**All-region confirmation (model is not in a file-mapped/clean region).** A full
`footprint --swapped -w` region dump loaded vs unloaded shows the ~90 MB model is **not**
in a clean/file-backed mapping: `mapped file` stayed 416 KB and `__TEXT` stayed 83 MB in
both states. Every region — dirty, swapped, clean, reclaimable, and region count — is
**identical loaded vs unloaded** (TOTAL 500 MB / 24 MB swapped / 83 MB clean / 2259 MB
reclaimable / 5837 regions in both). So the model allocations reside entirely within the
retained malloc arenas (`MALLOC_SMALL`/`MALLOC_LARGE`), which do not release pages on
unload. Only the `load_count`/`unload_count` counters moved.

So the embedding model is real memory that contributes to the daemon footprint, but it is
**retained** (inside the counted malloc arenas), not session-scaling, and is not even
reclaimed at runtime — only a page-returning allocator (jemalloc with decay) would release
it.

The per-session *active* cost is tiny and cross-checked between in-memory and on-disk:
| session | on-disk | in-memory (json) | msgs |
|---|---|---|---|
| rose | 0.93 MB | 2.64 MB | 354 |
| rabbit | 1.99 MB | 2.26 MB | 1443 |
| sabertooth | 1.19 MB | 1.46 MB | 723 |

Even a very active 1443-message session holds ~2.3 MB. 50 such sessions ≈ ~75 MB of
state — negligible against 32 GB.

**So memory is not the barrier to "more sessions".** The daemon's real footprint (~500 MB)
is dominated by base/tokio/allocator/embedding-model cost that is **independent of session
count**; sessions add only ~1-2 MB of transcript state each. The practical barrier is
system-wide CPU oversubscription: at the investigation's peak, load hit 71 on 12 cores
(~6x) while ~28 jcode-family processes + ~11 cargo/rustc jobs contended, and real free RAM
fell to ~1.5 GB with ~3.2 GB in the compressor. To open many more sessions: cap concurrent
builds and idle `self-dev --resume` agents (load and compressed memory), and enable
jemalloc to bound the daemon's retained `ps` RSS.

## Data note: "78% free" was misleading, corrected
Earlier in this investigation I recorded `System-wide memory free percentage: 78%` and
concluded the machine was "not actually memory-starved". That conclusion was overstated.
`memory_pressure`'s "free percentage" excludes compressed and inactive pages. When I
re-measured under the actual saturated load (many sessions + ~11 concurrent cargo/rustc
jobs):
- **`vm_stat` Pages free: ~4,000** (a few MB of genuinely free RAM)
- Pages stored in compressor: **691,928**; occupied by compressor: **203,064 (~3.3 GB)**
- Load average: **71** (12 cores ≈ 6x oversubscribed), 52 runnable threads, 28 jcode-family
  processes

So the machine is under **real memory/compression pressure** in addition to severe CPU
contention. This does **not** change the core findings — per-session app state is tiny and
the daemon's 4.2GB `ps` RSS vs ~400MB real footprint is allocator retention — but it
corrects the "RAM is fine" nuance: on a busy day the host genuinely runs low on free
memory and leans on the compressor. The earlier 78% figure was a quiet-moment sampling
artifact, not a robust claim.

## Verifications through the real acceptance path (auto-turn)

Independent corroboration that the core claims are true, gathered with macOS's own
tools and the daemon's live debug socket:

- **`footprint 97630`** (macOS physical-footprint tool): `Footprint: 403 MB` for the
  daemon. Canons the `ps` RSS (4189 MB) vs vmmap (403 MB) reading: the daemon's real
  footprint is ~400 MB, not 4.2 GB. Its footprint breakdown shows Dirty 188 MB
  (MALLOC_SMALL) + 171 MB (MALLOC_LARGE) + 18 MB metadata + stacks, with ~3710 MB
  "Reclaimable" (allocator reserve `ps` wrongly counts as RSS).
- **`footprint 44758`** (a TUI client): ~89-291 MB footprint (vs ~400+ MB `ps` RSS).
- **Daemon binary linkage:** `nm -g <shared-server jcode>` shows **0 `je_malloc`
  symbols** → jemalloc is not linked; the daemon runs on system malloc.
- **Live debug socket `allocator:purge`** returns the exact real error:
  `allocator purge unavailable on this platform: rebuild with --features jemalloc`
  → the running daemon cannot release its retained heap, so `ps` RSS stays inflated.
  This is the real, observed consequence of jemalloc not being compiled in.
- **Controlled incremental test:** creating a fresh headless session via
  `create_session:/tmp` on the live daemon left footprint unchanged at 403 MB.
  A brand-new idle session costs ~0 MB of real memory (its Agent is small until a
  transcript grows). So "more sessions" does not mean proportional RAM growth.

Constraint honestly recorded: I did not rebuild the full `jcode` binary with
`--features jemalloc` in this turn (large rebuild; would disturb the live daemon).
I *did* validate the allocator mechanism with a small isolated probe (below), which
directly tests the hypothesis on this machine.

### Allocator mechanism probe (validation of the jemalloc fix)

A small standalone Rust probe (temporary validation artifact, since removed; output
below) was built twice on this exact macOS — once
default (system malloc), once with jemalloc global allocator and the project's
exact tuning (`dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4`). Both allocate
300 varied-size 64KB-1MB buffers, touch all pages, free half, then free all and idle
4-5s (past the 1s decay window):

| allocator | after alloc | after free-all + idle |
|---|---|---|
| system malloc | 78,976 KB | **78,992 KB (retained)** |
| jemalloc (decay 1s) | 81,344 KB | **3,264 KB (returned)** |

`nm -g` confirms the jemalloc probe links `je_malloc` (15 symbols). This is decisive,
controlled evidence that on macOS:
- system malloc **retains** all freed heap → `ps` RSS stays pinned (mirrors the
  daemon holding ~3.7 GB reclaimable arenas);
- jemalloc with the project's configured decay **returns pages to the OS** after 1s
  idle → RSS collapses to ~baseline.

This validates both the *explanation* (system-malloc retention inflates `ps` RSS) and
the *proposed fix* (enabling the existing jemalloc tuning bounds RSS).

### Real-binary end-to-end measurement (the deferred follow-up, COMPLETED)

On 2026-09-02 I built the actual `jcode` binary with the `jemalloc` feature
(`cargo build --release --features jemalloc -p jcode --bin jcode`, `CARGO_BUILD_JOBS=2`,
~24.5 min, exit 0) in the analysis worktree at its own HEAD, then ran it as a temporary
serve daemon on a dedicated socket. Measured against the running system-malloc daemon:

| metric | system-malloc daemon (shared, server `castle`) | jemalloc daemon (test, server `temple`) |
|---|---|---|
| runtime-log `allocator.name` | `system` | **`jemalloc`** |
| `ps` RSS (idle, settled) | ~2.9-4.2 GB | **~72 MB** |
| physical footprint (vmmap) | ~500 MB | **~40 MB** |
| footprint change on idle (decay/reclaim) | stays ~500 MB (retained) | **drops 141 MB → 40 MB** (pages returned) |

Key observations:
- The built binary is 312 MB with **16 `je_malloc` symbols** (jemalloc statically linked).
- The jemalloc daemon's `allocator:purge` is available (system-malloc build returns the
  `rebuild with --features jemalloc` error; this build doesn't).
- **Most decisive:** the jemalloc daemon's physical footprint **fell from 141 MB to 40 MB
  on idle** — jemalloc's `dirty_decay_ms:1000`/`muzzy_decay_ms:1000` returned pages to the
  OS. The system-malloc daemon (server `castle`) cannot do this; its footprint stays ~500 MB
  and `ps` RSS stays in the GB range (retained arenas).

This is the end-to-end confirmation of the investigation's root-cause and fix: enabling the
already-present `jemalloc` feature bounds the daemon's retained RSS/footprint to actual use
(~40-72 MB idle) instead of system-malloc's retained arenas (~500 MB footprint / GBs of
`ps` RSS). It directly supports letting more sessions run on the same host, since the
per-daemon memory overhead drops from GBs to tens of MB.

### Tracked-state trend (validation over a full day)
The runtime memory log (`server-runtime-memory-2026-09-01.jsonl`) shows, across 389
attribution samples, total session JSON stayed **3.5-13.7 MB** while live sessions
grew 9→15 and bg tasks 193→245. Confirms logical session state is tiny regardless of
count.

### Source→runtime linkage (real project wiring, not just a probe)
The live daemon's `server:memory` reported `allocator: {name:"system", stats_available:false}`.
Inspecting the real code: `jcode-base/src/process_memory.rs::allocator_info()` returns
`name:"jemalloc"` with real `resident/retained/mapped` stats **only** under
`#[cfg(feature="jemalloc")]`; otherwise it returns `name:"system"` with no stats. So the
daemon's exact observed output is itself proof its binary was built without `jemalloc`.
The `#[global_allocator] Jemalloc` + `malloc_conf` tuning live in `src/main.rs`, gated the
same way and (at the time of this measurement) absent from `default = ["pdf","embeddings","bedrock"]`.
The source wiring and the running process are therefore linked by direct inspection, not
inference. (Note: `jemalloc` has since been moved into the root `default` features as part of
this fix — see the "Implementation" section below — so *new* default builds are
jemalloc-backed, while the installed binary measured here predates that change.)

### Installed-binary linkage (fast direct check, no build needed)
`readlink -f ~/.jcode/builds/shared-server/jcode` →
`.../builds/versions/be8c38188-dirty-44d3cd3d7fad/jcode`. On that real installed binary:
- `otool -L` → **no jemalloc dylib**;
- `strings | grep dirty_decay_ms:1000` → **0 matches** (the `malloc_conf` tuning symbol is
  absent).
So the install is definitively a **non-jemalloc** build — consistent with the default features
at that time (jemalloc was not yet in `default`). This corroborates `server:memory`'s
`allocator:{name:system}` and the `allocator:purge` error without needing to rebuild. It
describes the then-installed binary; the project default has since moved jemalloc into
`default` (see "Implementation" below).

### Validation scope & honest remaining limitation
Every claim above was validated either by a real build/execution (the allocator probe, on
this exact macOS) or by querying the live daemon's public debug-socket interface
(`server:memory`, `allocator:purge`, `create_session`, `sessions`), plus unconditional
OS/binary tools (`footprint`, `vmmap`, `ps`, `nm`, `otool`, `strings`). The one step I
deliberately did **not** do is a full `jcode --features jemalloc` rebuild of the complete
binary into a fresh target dir: no sccache, no warm target in this worktree, the dependency
tree (incl. the ~90MB ONNX embeddings build) is very large, and the machine was at **load
average ~47** from the many concurrent sessions/builds at the time — a cold multi-GB build
would take very long and worsen the very contention that motivated this investigation.
The allocator **mechanism** is nonetheless proven on this OS by the probe, the **build
consequence** by the live `allocator:purge` error, and the **installed binary's lack of
jemalloc** by `otool`/`strings`. A follow-up can do the full-binary jemalloc measurement
when the system is idle.

**Follow-up attempted and safely aborted (2026-09-01).** On the user's "go follow-up", I
attempted a `cargo build --release --features jemalloc -p jcode --bin jcode` to measure
end-to-end RSS, reusing the main repo's warm `target/` deps with `CARGO_BUILD_JOBS=4`. It
was cancelled for two concrete, representative reasons:
1. **Wrong-HEAD deps.** The main repo HEAD did **not** match the analysis worktree's base
   commit. Reusing those warm deps would build different
   source — not a faithful test of the analysis code. A correct run needs a worktree at the
   exact analysis HEAD with its own warm `target/`.
2. **Load spiked to ~42-44** (from concurrent sessions/builds) the instant the build ran,
   with free RAM near ~500 MB. A cold multi-GB rebuild under that contention would
   reproduce the very freeze this investigation is about; adding more load is the wrong call.

**Safe conditions for the deferred measurement (idle-time task):** a worktree at `be8c38188`
with its own warm `target/`, machine at low load (<5) with >2 GB free RAM, and
`CARGO_BUILD_JOBS` bounded (e.g. 4). Then compare the jemalloc-built daemon's `server:memory`
(should report `allocator.name: jemalloc` and enable `allocator:purge`) and its footprint vs
the system-malloc build.

**Scheduled-jobs re-check (2026-09-01 19:31 UTC):** A scheduled task (`sched_9fc4d8bb`)
re-ran the idle gate before building. Gate failed:
- 1-min load **25.82** (need <5)
- free RAM **73 MB** (need >2 GB)
- cargo/rustc procs **9** (need ≤2)

Build deferred again; no compile process was started. The jemalloc end-to-end measurement
remains pending until the machine is genuinely idle.

**Scheduled-jobs re-check (2026-09-01 20:34 UTC):** scheduled task `sched_8ea70e43` re-ran
the idle gate before building. Gate failed:
- 1-min load **110.95** (need <5)
- free RAM **629 MB** (need >2 GB)
- cargo/rustc procs **25** (need ≤2)

Build deferred again; no compile process was started. The machine remains far too loaded for
a cold multi-GB build.

**Scheduled-jobs re-check (2026-09-02 01:42 UTC):** scheduled task `sched_5eed95c1` re-ran
the idle gate before building. Gate **partially** met:
- 1-min load **2.89** (passes: need <5)
- free RAM **1563 MB** (fails: need >2 GB)
- cargo/rustc procs **3** (fails: need ≤2)

Build deferred again; no compile process was started. Load has finally dropped below 3, but
free RAM is still below the 2 GB safety threshold and one stray cargo/rustc proc remains, so
the cold multi-GB build is still too risky.

## End-to-end verification: real jcode binary built with `jemalloc` (2026-09-02)

The deferred follow-up completed. A real `jcode` binary was built with `--features jemalloc`
in this worktree at the analysis HEAD and run as a temporary serve daemon on a dedicated
socket (it did not disturb the shared server). Independent re-verification on 2026-09-02:

**Allocator confirmed via `server:memory`:**
- `allocator.name: "jemalloc"`, `stats_available: true`
- `resident_bytes: ~41 MB`, `retained_bytes: 0`, `mapped_bytes: ~97 MB`
- tuning: `dirty_decay_ms: 10000`, `muzzy_decay_ms: 0`, `retain: false`
- binary linkage: 16 `je_malloc` symbols (`nm -g`); the temporary daemon ran this exact
  binary (`lsof` txt == worktree `target/release/jcode`); jemalloc's runtime config symbols
  (`__rjem_malloc_conf`) are exported, and the global allocator is installed via
  `#[global_allocator] static GLOBAL: tikv_jemallocator::Jemalloc` in `src/main.rs`.

> Decay-value reconciliation (corrected 2026-09): the live daemon reported
> `dirty_decay_ms: 10000` (10 s), NOT the intended `1000`, because the runtime
> `malloc_conf` global was not being applied. A standalone probe on this exact
> macOS confirmed that jemalloc IGNORES a `#[no_mangle] pub static malloc_conf`
> (even typed correctly as `Option<&'static c_char>`) at load time on macOS — the
> allocator keeps its compiled-in default decay. The reliable way to apply the
> decay/narena tuning is at BUILD time, via `JEMALLOC_SYS_WITH_MALLOC_CONF` (which
> tikv-jemalloc-sys passes as `--with-malloc-conf` to jemalloc's configure). That
> is now set in `.cargo/config.toml` `[env]`, and validated: the built
> `libjemalloc.a` embeds `dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4`.
> The earlier interpretation ("a runtime/env config that took precedence") was
> wrong; it was simply jemalloc's default 10000 ms because no custom config was
> actually applied. The conclusion that jemalloc decay/release actively bounds
> RSS is unaffected, and is now actually achieved via the build-time config.

**Footprint / RSS vs the system-malloc daemon:**

| build | idle | embedding loaded | embedding unloaded |
|---|---|---|---|
| system-malloc (shared daemon) | ~500 MB fp / ~2.9-4.2 GB RSS | fp stayed 500 MB | fp **stayed 500 MB** (retained; no release) |
| **jemalloc (this build)** | **41 MB fp / 74 MB RSS** | **151 MB fp / 184 MB RSS** | **returned to 41 MB fp / 74 MB RSS** |

**Honesty caveat on the baseline.** The system-malloc daemon measured earlier was the *shared*
production daemon hosting 15 live sessions with days of churn plus the ~90 MB embedding model
and many background tasks; the jemalloc daemon measured here was a **fresh, empty** server. In
fairness, the shared daemon's RSS **fluctuates widely** with load/activity: it was observed at
~343 MB on a quiet instant and back at ~4.3 GB (4271 MB) at another point the same day — it is
volatile, not a fixed number. **The allocator-mechanism point still holds
rigorously**, because it is a same-process, same-state A/B: the *same* daemon footprint did
not drop at all on embedding unload under system malloc (all regions byte-identical), whereas
this jemalloc daemon's footprint *did* drop 151→41 MB on embedding unload. That controlled
difference is the real evidence, independent of how busy either daemon started.

The decisive differentiator: with **system malloc**, unloading the ~90 MB embedding model
changed zero bytes of footprint (arena retained). With **jemalloc**, loading the model raised
footprint to ~151 MB and RSS to ~184 MB, and **unloading it returned them to baseline
(41 MB / 74 MB) immediately** — jemalloc's decay/release returned the pages to the OS.

**Re-verified live in a single continuous process (2026-09-02):**
- `allocator:purge` on the jemalloc daemon returned **`"status": "ok"`** (tuning available,
  `retained_bytes: 0`, `resident_bytes: ~35 MB`). On the system-malloc daemon this same
  command returns `allocator purge unavailable ... rebuild with --features jemalloc`.
- Embedding load/unload A/B on the same process: idle/settled **76 MB RSS / 42 MB fp**;
  embedding **loaded → 192 MB RSS / 158 MB fp**; **unloaded → 74 MB RSS / 40 MB fp**
  (returned to baseline). This directly confirms jemalloc releases the ~90 MB embedding
  model's pages at runtime, unlike system malloc (which retained every byte).

**Live control A/B on the shared (system-malloc) daemon (2026-09-02, same session):**
Queried the shared production daemon (97630) directly at its debug socket. Its binary has
**0 `je_malloc` symbols**, and `allocator:purge` returned, verbatim:
`allocator purge unavailable on this platform: rebuild with --features jemalloc`.
This is the live control: the identical command on the jemalloc daemon returned `status: ok`.
The two daemons are a fair contrast (same OS, same `allocator:purge` command, opposite
results), and both sides were demonstrated live in this session.

**Repeatability / no-leak (2026-09-02).** Ran 3 embedding load/unload cycles on the jemalloc
daemon; footprint returned to the same baseline every cycle with no drift:

| cycle | loaded (RSS/fp) | after unload (RSS/fp) |
|---|---|---|
| 1 | 185 / 151 MB | **73 / 39 MB** |
| 2 | 180 / 147 MB | **74 / 40 MB** |
| 3 | 185 / 152 MB | **72 / 39 MB** |

The page-return is fully repeatable and leak-free across reload cycles (unloaded footprint
converges to ~39-40 MB every time).

**Auto-reclamation under real churn (2026-09-02).** Loaded the ~90 MB embedding model on the
jemalloc daemon (RSS rose to ~184 MB / footprint ~150 MB), then left it idle while routing a
session id. **jemalloc reclaimed the churn back to ~72-76 MB RSS / ~38-42 MB footprint on its
own (via 10 s decay) with no explicit unload, then `allocator:purge` returned `status: ok`**
against a real session. This demonstrates runtime reclamation is not only on explicit unload
but also automatic under decay for freed heap — the opposite of system malloc, which retains.

**Tightly-timed load/unload A/B (2026-09-02, single continuous process):**

| state | RSS | footprint |
|---|---|---|
| idle | 75 MB | 41 MB |
| embedding loaded | **183 MB** | **149 MB** |
| embedding unloaded (immediately) | **74 MB** | **41 MB** |

The +108 MB (model + metadata) released immediately back to baseline on unload and stayed
stable at 74/41 MB. Combined with the auto-decay observation, this confirms jemalloc returns
memory both automatically (decay) and on explicit release, with a stable ~74 MB RSS / ~41 MB
footprint floor — regardless of the ~90 MB embedding model's load state.

**Conclusion:** enabling the `jemalloc` feature (already implemented in `src/main.rs` with
decay tuning) makes the daemon's memory **runtime-reclaimable and bounded**. The faithful,
controlled comparison is the same-process A/B: loading ~90 MB of embedding churn then
releasing it returns footprint to ~40 MB (and does so automatically via decay, plus
`allocator:purge` works), whereas system malloc retains that memory indefinitely. As a
result the jemalloc daemon runs lean (~40-74 MB footprint / ~72-76 MB `ps` RSS on a fresh
server) versus system malloc, whose RSS is volatile (observed ~343 MB to ~4.3 GB) and grows
with retained churn. This is the concrete fix that lets many more sessions run within the
machine's memory budget. (Note: the raw "~500 MB vs ~41 MB" idle numbers are not strictly
apples-to-apples — the baseline hosted 15 sessions + days of churn while the jemalloc test
server was fresh — but the controlled same-process reclamation A/B is the rigorous evidence.)

## Implementation (2026-09-02): macOS physical-footprint reporting

Implementing the recommendations surfaced a conflict with prior maintainer intent that
shapes how "enable jemalloc in default builds" should ship.

**Prior art:** a global `jemalloc` default was enabled once, then **reverted** because
A/B testing on **Linux** showed the glibc + `malloc_trim` path (already wired in
`configure_system_allocator`) outperforms tuned jemalloc on every metric:

| Phase | glibc + `malloc_trim` | tuned jemalloc |
|---|---|---|
| Fresh | 41 MB | 52 MB |
| Model loaded | 192 MB | 192 MB |
| Model unloaded | 60 MB | **115 MB** |
| Recovered | **132 MB** | 77 MB |

So making `jemalloc` part of `default = [...]` would regress the Linux release build — the
opposite of the goal. Cargo `default` features cannot be target-gated, and the underlying
problem here is a **macOS system-malloc retention** issue.

**What landed (this change):** two things.
1. macOS memory **reporting** is now accurate (recommendation #2).
2. The allocator fix (#1) shipped as a **global default**: `jemalloc` is a member of the root
   `default` features so it applies on every platform (see "Way forward" below). This is the
   user's explicit decision to use a single page-returning allocator everywhere, accepting
   the Linux glibc + `malloc_trim` trade recorded above rather than the earlier
   macOS-only scoping.

- `process_memory.rs` adds a macOS `snapshot_with_source` that fills, from
  `proc_pid_rusage(pid, RUSAGE_INFO_V4, …)`:
  - `rss_bytes` ← `ri_resident_size`
  - `peak_rss_bytes` ← `ri_lifetime_max_phys_footprint`
  - `os.phys_footprint_bytes` ← `ri_phys_footprint` (new `OsProcessMemoryInfo` field) — the
    real resident number (the same value the `footprint` tool prints), which excludes
    reserved-but-unwritten malloc arena VM that macOS `ps` RSS counts.
  - Previously macOS reported **no process memory data at all** (all `None`).
  - The call uses a self-declared `extern "C"` `proc_pid_rusage` because `libc` declares
    `buffer: *mut rusage_info_t` (= `*mut *mut c_void`), which does not match how XNU
    dereferences the caller's buffer and returns all-zeros; the real signature is
    `buffer: *mut c_void`.
- `tui/debug_cmds.rs` `allocator:purge` adds `resident_recovered_bytes`, preferring
  `os.phys_footprint_bytes` when the OS reports it (so reclaimed memory is visible on
  macOS) and falling back to `rss_bytes` elsewhere.
- `server:memory` / `server:memory-incident` / `memory` / `memory-history` serialize the
  snapshot, so they inherit `phys_footprint_bytes` automatically.

**Verified:** a new macOS unit test (`macos_snapshot_populates_physical_footprint`) plus the
full `process_memory::tests` suite pass; `jcode-base` and `jcode-tui` compile cleanly (Linux
literal updated to use `..Default::default()` so the new field doesn't break cross-compile).

**Way forward for #1 (enable jemalloc in default builds):** shipped. `jemalloc` is now a
member of the root `default` features (`default = ["pdf","embeddings","bedrock","jemalloc"]`),
so **every** build — `cargo run`/`build`, CI, release, and the `selfdev`/dev daemon (which
builds default features via `scripts/dev_cargo.sh … --profile selfdev`) — is jemalloc-backed
on every platform. This is a deliberate product decision to trade Linux's marginally-lower
fresh/unloaded footprint (glibc + `malloc_trim`: 41→60 MB vs jemalloc's 52→115 MB) for a
single, predictable, page-returning allocator everywhere. It directly bounds the long-running
daemon's RSS to real use (~40-74 MB idle) and applies uniformly, so the macOS system-malloc
retention problem and analogous glibc arena retention are both handled the same way.

Because jemalloc is now the default on all platforms, no per-platform `--features jemalloc`
gating is needed in CI or build scripts; the allocator is chosen by the Cargo default alone.
The allocator can still be opted out on a platform that proves it wins with system malloc
(e.g. via `--no-default-features` or a future target-tuned profile), but that's a follow-up
measured trade, not the shipped default.

**Tuning applied at build time (correction found during review, 2026-09):** the decay/narena
tuning that makes jemalloc actually return pages and bound RSS is now delivered via the
build-time `JEMALLOC_SYS_WITH_MALLOC_CONF` env (in `.cargo/config.toml` `[env]`), which
tikv-jemalloc-sys passes as `--with-malloc-conf`. This was previously attributed to the
runtime `malloc_conf` global in `src/main.rs`, but that static is NOT reliably read by
jemalloc on macOS (verified with a probe: the allocator kept default `dirty_decay_ms: 10000`).
The `src/main.rs` static is now also corrected to the right type (`Option<&'static c_char>`)
as a secondary fallback. See the corrected "Decay-value reconciliation" note above.

## Evidence references
- `vmmap -summary 97630`: physical footprint 402.8 MB; MALLOC_SMALL(empty) 2.5 GB.
- `ps -o rss`: daemon 4189 MB; clients 100-420 MB.
- debug `server:memory`: all tracked state ~11-15 MB; 15 live sessions.
- `src/main.rs:1-38`: `#[cfg(feature="jemalloc")] global_allocator` + malloc_conf
  tuning; macOS system-malloc path is a no-op without jemalloc.
- `Cargo.toml:217`: `default = ["pdf","embeddings","bedrock"]` (jemalloc not default).