# jcode vs `PrimeIntellect-ai/prime-agent`: comparison research

**Status:** Research (informational; no adoption implied)
**Date:** 2026-09-04

## Purpose

This document compares jcode and
[`PrimeIntellect-ai/prime-agent`](https://github.com/PrimeIntellect-ai/prime-agent)
as coding / research agent harnesses. It covers positioning, architecture, the agent
loop and control surface, subagents/recursion, persistence, long-running and autonomous
work, memory, skills, and self-modification (jcode self-dev and prime-agent's Continual
Harness), and finally what is genuinely transferable versus what is a divergence. It is
written as research, not as a proposal to adopt any specific prime-agent pattern. The report takes jcode's
vantage point (it lives in the jcode tree): "worth studying" (see §10) means the idea
is promising for jcode, and "what NOT to take" (see §11) means a pattern jcode should
preserve its difference from. This one-directional lens is deliberate and not a claim
that prime-agent's design is better or worse overall.

**On evidence.** Quoted strings fall into two kinds: verbatim source quotes (e.g. jcode's
"The most RAM efficient harness") and the doc's own short labels for a design idea applied
to a specific project (e.g. "writes programs"). Context makes the distinction clear; the
§12 Sources section lists the exact docs the claims derive from. Where this report
recommends or declines a direction, it says so explicitly rather than dressing analysis
as fact.

> **Scope note on the continual/self-improving harness.** prime-agent's headline
> differentiator is *self-improvement*: a `Continual Harness` where `/refine`
> rewrites supplemental harness state (memories, skill descriptions, subagent
> specs) against the observed trajectory. This report describes that mechanism in depth
> in **§9 Self-modification** (and touches on memories in **§7**). jcode's
> self-evolution axis is binary **self-dev** (the agent edits, builds, and reloads its
> own source), which is **related but distinct** from prime-agent's harness-state
> `/refine`. Adopting `/refine`-style harness-state refinement in jcode is possible
> without adopting a REPL kernel, but it is a new direction that needs explicit user
> sign-off before design work.

## At-a-glance

| | **jcode** | **prime-agent** |
|---|---|---|
| Language | Rust (single binary, many crates) | TypeScript (Node) host + Python kernel |
| License | MIT | MIT |
| Positioning | Taglines: "The most RAM efficient harness" / "The most intelligent harness" | "Self-improving RLM harness" for coding & long-running research work |
| Control surface | Tool-calling agent loop (grep/compass, memory, swarm tools, browser, MCP) | Persistent Python REPL as the *only* built-in model tool |
| Subagents | Multi-agent server collaboration; agents share one repo & are notified when a file they read is edited | `rlm(...)` spawns real child `AgentSession`s (independent context + session dir) |
| Daemon model | Single server, multi-client; sessions live on server, TUI reconnects | Supervisor → resident per-session worker; client detach/reattach |
| Recursion | Agent-spawned worker teams (coordinator → workers), headless or headed | RLM delegation over a host bridge; depth-bounded |
| Memory | Semantic-vector memory graph, auto-extract, session search, ambient consolidation | Harness state (memories) editable via `/refine`; per-session by default |
| Self-modification | Binary self-dev (edits/builds/tests/reloads its own code) | Continual Harness: agent refines harness *state* via `/refine` (§9) |
| Skill format | Discovery/embedding-gated activation + slash commands | Agent-Skills markdown + Python-backed executable skills |
| Cross-harness resume | Imports/resumes Codex, Claude Code, OpenCode, Cursor, Pi sessions | N/A (upstream lineage is `pi`/pi-mono) |
| Extensibility | Rust crates + self-dev + MCP + provider config | JS/TS extensions, Python skills, Prime Agent packages |

## 1. Positioning

Both projects ship a MIT-licensed terminal agent that works in the current directory,
runs commands, and edits files. That is where the resemblance ends.

**jcode** is a performance-focused, single-binary Rust harness. Its marketing centers
on RAM efficiency (27.8 MB PSS for one session), render speed (1000+ fps, a hand-written
mermaid renderer ~1800x faster than the JS one), and a multi-session server model. It
treats the agent as the driver of its own extensibility via **self-dev** (modify, build,
test, reload its own binary).

**prime-agent** is a TypeScript host with a persistent **Python control kernel**. Its
headline is a *Recursive Language Model (RLM)* runtime plus a *Continual Harness* that lets
the agent refine its own durable harness state. It targets evaluations and long-running
autonomous research/coding tasks, with daemon-backed sessions, heartbeats, schedules,
persistent goals, and bounded autonomous mode.

The core architectural difference: **jcode is a conventional tool-calling agent with a
server-centric process model and self-modifying binary; prime-agent is a REPL-centric
agent whose model "writes programs" (Python) and whose harness state is itself editable
data.**

**What it can give jcode:** a clarifying frame for jcode's own strengths — confirming that a
single binary + tool-calling surface is itself the differentiation, and that any
REPL/programmatic-control idea from prime-agent should be considered *in addition to*, not
instead of, that identity.

## 2. Architecture

### jcode (Rust, single server)

```
SERVER (jcode serve)
  ├─ Unix socket + debug socket
  ├─ Provider(s) + shared MCP pool
  └─ Sessions (multiple, named e.g. fox/bear/owl)
        │
        ▼
TUI clients  (connect / reconnect / attach)
```

- **Single server, many clients.** One Rust process owns all sessions and state; TUI
  clients connect over a Unix socket and reconnect transparently after disconnect or
  server reload. Multi-client attach to the same server is central.
- **Shared MCP pool** across sessions; providers configured per profile.
- **Swarm.** Multiple agents in the same repo are orchestrated by the server. When agent
  A edits a file agent B has read, the server notifies B (code-shifting-under-feet)
  with a diff. Agents DM / broadcast over server-hosted channels. (See §4 for the full
  swarm/recursion treatment.)
- **Memory** runs as a side-service: every turn is embedded, retrieval hits feed context
  or a memory sideagent verifies relevance; memories extracted and consolidated in the
  background (ambient mode).

### prime-agent (TypeScript host + Python kernel + daemon supervisor)

```
Daemon supervisor (routing · attachments · recovery)
  ├─ Catalog process (saved-session scans)
  └─ Session worker (one per root session tree)
       └─ AgentSessionRuntime
            ├─ Root AgentSession
            ├─ Scheduler
            ├─ Root Python kernel
            └─ RLM child runtimes (session + optional kernel)
```

- **Client owns rendering only; execution lives daemon-side.** Closing the TUI detaches
  the client; the resident worker keeps the session, kernel, schedules, and subagents
  alive. Reattach with `prime-agent attach`.
- **AgentSession** owns provider calls, queues, tools, compaction, goals, child
  lifecycles, transcript writes. It is "generation-aware" when streaming events.
- **Persistent Python kernel** is the model-facing control environment. A typed host
  bridge (`rlm.host_request`) keeps credentials, provider execution, scheduling, and
  transcript writes in the TypeScript host while Python *drives*.

**Summary:** jcode's server is process-centric and multi-client; prime-agent's split is
presentation-vs-execution with a *kernel* in the middle. jcode multiplexes many sessions
in one server; prime-agent isolates one session tree per worker under a supervisor.

**What it can give jcode:** the *host-owns-state* principle — keeping credentials, provider
calls, and transcript writes outside the execution surface — is the one portable idea
(prioritized in §10 item 1). The daemon-supervisor + per-session-worker tree is **not** worth
copying: jcode's single server already delivers the same continuity more simply.

## 3. The agent loop / control surface

**jcode:** a standard tool-calling loop. Tools include a purpose-built grep
(`agentgrep`) that enriches matches with file-structure info (function names,
displacement) so the agent can infer code without reading it, and adaptively truncates
returns by what it has already seen. There is also a semantic code-search tooling
(`compass_query` over a knowledge graph, see `docs/COMPASS.md`) as the preferred path
for structure/relationship queries. Skills are injected by embedding similarity instead
of all being loaded at startup.

**prime-agent:** one built-in model tool — the `ipython` REPL. Rather than exposing a
tool per capability, the model writes Python: `bash()`, path/fs inspection, transforms,
skill calls, and subagent delegation all flow through the persistent kernel. Python state
(variables, imports, cwd, env) survives compaction and turns. This collapses many
built-in tools into one programmatic surface.

This is the sharpest design divergence: **"many typed tools" vs "one programmatic
REPL."** jcode optimizes the input side (context-efficient, structure-aware grep, buffer
management) and keeps tools distinct; prime-agent optimizes for an unbroken programming
environment where durable state is ordinary Python.

**What it can give jcode:** the *persistent-programming-surface* idea (long-lived, stateful
execution between calls) is the most interesting transfer — not as a replacement for jcode's
typed tools, but as an optional scripting sandbox *underneath* them. This stays consistent
with §11's "not a REPL-as-the-only-tool" position: the typed tools remain the surface.

Other surface capabilities are comparable but not core differentiators:
- **Non-interactive / headless output.** jcode has `jcode run '<prompt>'`; prime-agent adds
  `-p/--print`, `--mode json`, and `--mode rpc`. The structured JSON/RPC output modes are the
  only portable idea (for scriptable/external integration).
- **Browser automation.** jcode ships a Firefox Agent Bridge; prime-agent reaches browsers via
  MCP-backed skills or extensions. Functionally equivalent; nothing structural to borrow.
- **Rendering.** jcode's custom mermaid renderer and side panels are ahead; prime-agent uses a
  standard TUI. Nothing to borrow here.

## 4. Subagents and recursion

**jcode swarm:** agents in the same repo collaborate through the server, with file-read
conflict notification and diff inspection. Recursion is agent-spawned: the swarm tool
turns the main agent into a coordinator and spawns workers. Messaging channels and
completion statuses are managed automatically; runs can be headless or headed.

**prime-agent `rlm()`:** `rlm(...)` in the kernel is a programmatic call that spawns a
real child `AgentSession` (independent context + session dir) and returns an admission
handle immediately — never waiting for the child's answer. Results arrive via
`agent_message` or files. Children inherit provider, skills, tools; handles survive
compaction/kernel-restart; `rlm.list_subagents()` / `agent_message.send(...)` follow up
or address siblings. Direct agent-to-agent messaging and a family roster are routed by
the daemon supervisor.

Both support multi-agent work. jcode frames it as *server-mediated collaboration*;
prime-agent as *recursive programmatic delegation*.

**What it can give jcode:** jcode's swarm is repository-scoped; prime-agent's `rlm()` offers a
lighter, task-scoped delegation unit with a **first-class spawn handle and a parent-scoped
child registry** that survives restarts. jcode could borrow the *explicit child handle/registry*
idiom (child ids, names, session dirs, status) as a standard shape for its own swarm
subagents.

## 5. Persistence and sessions

- **jcode:** sessions are managed server-side (`~/.jcode`), resumable, cross-harness
  (jcode can import and resume Codex, Claude Code, OpenCode, Cursor, and Pi transcripts).
  Server reload preserves state; clients reconnect.
  An event-sourced session log is an in-progress design direction (scaffolded in
  `crates/jcode-base/src/session/event_types.rs`, not yet end-to-end); it would contrast
  with prime-agent's append-only flat JSONL files by being log-bracketed and replayable.
- **prime-agent:** sessions are flat JSONL under `~/.prime/agent/sessions/`, written by
  the worker; artifacts per session. Session branching via `/fork` (CLI `--fork`) and
  `/clone`, `/tree` navigation, `/compact`
  with custom instruction, HTML export and gist `/share`. Detached sessions and schedules
  persist across restarts; due scheduled ticks are claimed before delivery so a crash
  does not replay an uncertain prompt, and missed ticks are coalesced rather than backlogged.

**What it can give jcode:** cross-harness import/resume is already a jcode strength. The
portable pieces are prime-agent's **session-tree navigation** (`/tree`, `/fork`, `/clone`) as
first-class operations, and the **two-phase scheduled-tick delivery** (claim-before-deliver,
coalesce misses; see §10 item 3) that jcode's ambient/background scheduler could adopt for
recurring prompts.

## 6. Long-running & autonomous work

prime-agent makes long-running work an explicit product surface:

- **Heartbeats** (`/heartbeat`, programmatic `rlm_heartbeat`) re-enter a session
  periodically with `steer`/`follow_up` delivery.
- **Schedules** (`prime-agent schedule add ... "0 9 * * 1-5"`) one-time or cron prompts,
  persisted per session.
- **Persistent goals** (`/goal`) keep an objective in context until complete/paused;
  token/elapsed-time accounting, optional token budget.
- **Autonomous mode** (`/autonomous`, or `--autonomous-*` CLI) is a bounded host policy:
  continuations until quality gates (`--autonomous-gate "npm run check"`) pass or
  continuations/turns/tokens/wall-clock limits are hit. Gates avoid rerunning an
  unchanged failed gate.
- **Compaction** is automatic and kernel-persistent; `compact.run()` is programmatically
  callable.

jcode covers autonomous single-shot runs (`jcode run`), multi-client attach, and
server-side continuity, but frames goals/schedules/heartbeats differently: it has no
user-facing `/schedule` or `/heartbeat` surface or `/goal`-style accounting. Long-horizon
continuation instead uses ambient/background scheduling and a timed `/overnight` run,
rather than per-session recurring prompts and persistent goal state. The quality-gate
autonomous loop is the most novel prime-agent behavior jcode does not currently expose
as a first-class host policy.

**What it can give jcode:** three concrete ideas — (1) **goals-with-budgets** (durable
objective + token/wall-clock accounting) to complement jcode's ambient/`/overnight`;
(2) **quality-gate autonomous completion** (a gate that must pass before declaring the run
done, and not re-running an unchanged failed gate; see §10 item 2) for headless runs; and
(3) **recurring user and programmatic heartbeats** with steer/follow-up delivery.

## 7. Memory vs harness state

- **jcode memory:** automatic and retrieval-first. Every turn is embedded; a cosine-similarity
  graph query surfaces related memories; a memory *sideagent* can verify relevance and do
  extra retrieval before injection. Extraction is background (semantic drift, K turns,
  session end). Explicit memory tools + session search + ambient consolidation. This is
  *generated by the harness automatically*.
- **prime-agent harness state:** durable supplemental prompts, memories, skill
  descriptions, and subagent specs; the agent can **refine** them deliberately via `/refine`
  (mechanics detailed in §9). State is local to the session by default. This is *edited by
  the agent deliberately*.

They are complementary notions: jcode pushes automatic semantic memory; prime-agent
pushes deliberate, recordable self-correction of harness state. The two are examined
side-by-side in §9; adopting prime-agent's harness-state refinement in jcode is an open
design question (see §10 item 6), distinct from jcode's binary self-dev.

**What it can give jcode:** the distinctive idea is to layer a **snapshot-rollback, deliberate
revision path** on top of jcode's *automatic* memory accumulation — letting the agent actively
correct memories instead of only growing them. It is the lowest-risk way to add
self-correction without touching the runtime; see §10 item 6 for the prioritized treatment.

## 8. Skills

- **jcode:** skills are loaded lazily via embedding hit (like memories), or manually via
  a skill tool / slash commands. Available skills: `/optimization`, etc.
- **prime-agent:** Agent-Skills markdown (`SKILL.md`) plus **Python-backed skills** — an
  installable Python package exposed by import name, a superset of instruction-only
  skills (typed callables, deps, optional shell). Skill loading gated on `SKILL.md`
  metadata in the startup prompt; full doc loaded when the task matches (progressive
  disclosure). A built-in `skill-creator` builds new markdown or Python-backed skills; the
  continual-harness `/refine` separately persists just the *description* of a newly
  recurring call, not the packaged skill.

jcode is embedding-driven; prime-agent is programming-surface-driven. Both do lazy,
task-gated skill loading.

**What it can give jcode:** a **declarative `SKILL.md` metadata format** (name, description,
frontmatter) as a portable skill manifest to compare against jcode's embedding-gated
activation; and the idea of **Python-backed skills** — reusable *executable* packages, on the
condition jcode ever adds a scripting/runtime surface (parked otherwise, see §10 item 5).

## 9. Customizability and self-modification

Both harnesses are highly self-modifiable, but at different layers. This is where
prime-agent's "self-evolving" claim and jcode's "self-dev" are **related but not the
same** — it is worth separating the two axes precisely.

### jcode self-dev: modifying the binary

- **jcode** treats the agent itself as the thing to evolve. In self-dev mode the agent
  edits, builds, and tests its own Rust source, then reloads its own binary and continues
  work in (potentially many) sessions automatically. It is binary-level, irreversible-ish,
  requires a full compile/reload cycle, and jcode recommends a frontier model for it.
  This is a *program-under-change* model: the agent changes the code that runs the agent.

### prime-agent Continual Harness: modifying durable harness *state*

- **prime-agent** does not self-modify its binary. Its "self-improvement" is the
  **Continual Harness**: `rlm.harness` is a persisted state ledger holding prompt notes,
  memories, reusable skill descriptions, sub-agent specifications, and refinement events.
  `/refine` runs a dedicated review over the current trajectory and applies **small
  create/update/delete edits** to that supplemental state. Rollback is by recorded
  before/after snapshots, and the **base system prompt is never rewritten** — refinements
  are always supplemental.
- State homes: session-local at `session-artifacts/<id>/harness/harness_state.json`,
  global entries under `~/.prime/agent/harness/`. It is *state*, not code.
- Scope discipline: per skills.md, `/refine` persists just the *description* of a
  repeatedly-emerging Python call; it does **not** replace packaging real reusable
  functionality as a Python-backed skill (`skill-creator`). So prime-agent's evolution is
  deliberately conservative: it grows *descriptions and memories*, not arbitrary code.

### The relationship

The two are complementary, not competing:

| | jcode self-dev | prime-agent Continual Harness |
|---|---|---|
| What changes | its own Rust binary | supplemental harness state (prompts, memories, skill/subagent descriptions) |
| Unit of change | code + reload | small create/update/delete ledger edits with snapshots |
| Circuit | edits → builds → tests → reloads → continues | reviews trajectory → applies evidence-backed refinements → rollback-able |
| Who can improve it | the agent (with a frontier model) | the agent via `/refine` |
| Baseline invariant | your working source tree | the immutable base system prompt |
| Risk | breaking the running program | corrupting its own memories/skill metadata (mitigated by snapshot rollback) |

The honest framing: prime-agent's self-evolution is *low-risk, evidence-gated state
editing* that stays far from code; jcode's self-dev is *high-ceiling, high-risk code
mutation*. They address different failure modes and could in principle coexist — jcode
could adopt a harness-state-refinement mechanism without adopting a REPL kernel.

**What it can give jcode:** the coexistence insight is the takeaway — jcode can keep its
binary self-dev *and* add a lightweight, snapshot-rollback **harness-state refinement**
layer (noted in §7 and §10 item 6) without giving up single-binary/Rust simplicity.

## 10. What is genuinely worth studying (not necessarily adopting)

For each, the jcode-relevant takeaway is stated; adoption is separate from analysis.

1. **The typed-host / kernel bridge.** prime-agent keeps credentials, provider calls,
   transcript writes, and scheduling out of the Python kernel while still exposing a
   programmatic model interface (`rlm.host_request`). jcode has no kernel; its analog is
   policy/tool types. The *separation of "model-facing surface" from "host-owned state"*
   maps well onto jcode's tool/state boundaries (message types, side-panel, memory types).
   *Decision:* adopt as a design principle in future tool/state seams; no new runtime.
2. **Quality-gate autonomous loop.** `--autonomous-gate "npm run check"` as a bounded,
   evidence-driven continuation policy that refuses to re-run an unchanged failed gate
   aligns with how jcode already reasons about verification in its own workflows, and is a
   clean way to formalize evidence-backed completion for headless runs.
   *Decision:* strongest candidate to prototype (bounded gates in `jcode run`/`/overnight`).
3. **Two-phase scheduled ticks** (claim-before-deliver, coalesce missed ticks). A concrete,
   low-risk delivery pattern for recurring prompts. It is related to — but distinct from —
   jcode's documented "durable inbox" research direction (message routing, from the
   deepseek-harness takeaways); prime-agent applies the claiming idea to *scheduled
   prompts* rather than messages. *Decision:* worth borrowing for any future recurring-prompt
   scheduler; do not conflate it with the durable-inbox message-routing work.
4. **Lazy skill loading that is *explicitly* skills-owned.** jcode already does
   embedding-gated skill injection; prime-agent shows an alternative: only metadata in
   startup prompt, full doc loaded on task match. Cheap to compare against jcode's
   current embedding approach. *Decision:* benchmark against jcode's current approach
   before changing anything.
5. **Python-backed skills as a superset format.** jcode has no Python execution surface;
   this is only relevant if jcode later adds a scripting kernel. *Decision:* park unless a
   scripting/runtime surface is pursued.
6. **Evidence-backed harness-state refinement (`/refine`).** Separately from jcode's
   binary self-dev, prime-agent shows a cheap, reversible way to let the agent improve
   *supplemental state*: snapshot-rollback ledger edits (prompt notes, memories, skill and
   sub-agent descriptions) with the base prompt left immutable (§9). Because it touches
   only state, not the running binary, it is far lower-risk than self-dev and needs no
   REPL. *Decision:* the most promising new axis for jcode if user sign-off is given —
   prototype a bounded, session-local `/refine`-style refinement over jcode's existing
   memory/knowledge state before touching anything code-generating.

## 11. What NOT to take (divergences to preserve)

- **REPL-as-the-only-tool.** jcode's many-typed-tools model (structure-aware grep,
  embedded skill injection, memory sideagent) is a deliberate design that fits a Rust
  single-binary harness. Replacing it with a Python REPL would mean a new runtime and a
  loss of the context-efficiency work jcode is optimized for.
- **Replacing jcode self-dev with the Continual Harness's `/refine`.** jcode's
  self-modification is binary self-dev; prime-agent's is harness-state editing. These
  are complementary, not interchangeable — do not *replace* self-dev with `/refine`.
  (Whether jcode should *also* add a harness-state-refinement layer is a separate,
  open question — see §10 item 6.)
- **A supervised per-session worker process tree.** jcode's single-server, multi-client
  model already provides continuity and attach; adding a supervisor + per-session worker
  + kernel tree would add complexity without a current gap driving it.
- **A Node/TypeScript host (and an optional bundled Python kernel runtime).** prime-agent
  ships a JS/TS host plus a Python kernel; jcode is a single Rust binary. Adding them would
  directly conflict with jcode's "single binary, RAM-efficient, Rust" identity.

## 12. Sources

- prime-agent README, `architecture.md`, `rlm.md`, `rlm-runtime.md`, `usage.md`,
  `long-running-agents.md`, `skills.md` (fetched from `PrimeIntellect-ai/prime-agent@main`).
- jcode `README.md`, `docs/SERVER_ARCHITECTURE.md`, `docs/MEMORY_ARCHITECTURE.md`, `docs/COMPASS.md`,
  `docs/RESUME_BEHAVIOR.md`, `docs/research/deepseek-harness-takeaways.md`, and crate/package layout
  in the local checkout. The cross-harness import/resume list (incl. Cursor) is verified in
  `crates/jcode-session-types/src/lib.rs` (`ResumeTarget`) and `crates/jcode-tui-session-picker/src/lib.rs`
  (`SessionFilterMode`), which the README's four-item list does not fully capture.
