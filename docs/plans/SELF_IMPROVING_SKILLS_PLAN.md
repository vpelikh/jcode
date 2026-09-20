# Plan: Self-Improving Skills

> Status: Proposal. Companion to `MEMORY_ARCHITECTURE.md` and `CRATE_OWNERSHIP_BOUNDARIES.md`.
> Inspirations: Nous Research **hermes-agent** (`agent/background_review.py`,
> `tools/skill_manager_tool.py`, `tools/skill_linter.py`, `agent/curator.py`,
> `tools/skill_usage.py`) and **hermes-agent-self-evolution**
> (`evolution/core/constraints.py`, `fitness.py`, `external_importers.py`).
> Scope: make jcode's skills *learn from the user's own turns*, not just be
> loadable artifacts.

## Problem / current reality (verified in code)

jcode already has two of the three pieces of a learning loop:

1. **Memory that learns**: `MemoryAgent` (`crates/jcode-base/src/memory_agent.rs`) runs a background `process_context` → LLM `extract_from_context` pipeline on
   every fresh user turn and surfaces relevant memories. It is the *existing
   seam* for "review the turn after the fact."
2. **A skill system**: `SkillRegistry` in `crates/jcode-base/src/skill.rs`
   loads `SKILL.md` from `~/.jcode/skills/`, `~/.agents/skills/`, and Claude
   plugin dirs. The `skill_manage` tool (`crates/jcode-app-core/src/tool/skill.rs`)
   can `load/list/reload/reload_all/read`, and project-local skills are a
   per-session overlay.

What jcode **cannot** do today:

- **Create** a skill from a session.
- **Patch/improve** an existing skill during use.
- **Assess** whether a loaded skill is well-formed (a stub, placeholder-filled,
  or structureless skill silently enters the registry and the system prompt).
- **Measure** whether a change actually improved a skill.

So jcode has "skills that exist" but not "skills that improve." Hermes shows a
complete, battle-tested design for the missing piece, and jcode has the right
seams to host it.

## Goal

Introduce a **closed self-improvement loop**: after a turn (or periodically),
a background review decides whether a technique, correction, or preference is
durable, then either patches an existing skill or creates a new class-level
skill, bounded by a quality gate and a judge so only *better* skills stick.
This is the missing third piece: jcode has memory and a skill system, but no
way to turn a turn into a better skill.

This is an **optional, intentionally opt-in** feature: auto-editing the skill
library runs only when the user enables it, so it never mutates skills
unprompted. The self-improvement pass is off by default.

## Design pillars (adopted from Hermes)

These are the non-negotiables before shipping auto-editing. Skipping any of
them turns "self-improvement" into "an agent mutating the user's files
unprompted." They also depend on each other: the protection model (pillars 2
and 3) must exist before the review agent (Phase 2) can write anything.

1. **Fork isolation**: the review runs with a *restricted toolset* (skill +
   memory tools only). No terminal, no arbitrary edits. (Hermes
   `background_review.py`: "everything else is denied at runtime".)
2. **Protected-skills policy**: endorsed/catalog, hub-installed, pinned, and
   user-authored skills are off-limits to the automatic pass. Only explicit
   jcode-managed skills may be auto-edited. Without this, self-improvement
   corrupts the user's own work.

   > Note: jcode does **not yet** track provenance on loaded `Skill`s. Today it
   > only has a static `EndorsedSkill` catalog (`endorsed_skills()` in
   > `skill.rs`) and treats every on-disk skill equally. The `jcode-managed` /
   > `pinned` / `hub-installed` distinctions, and the enforcement itself, are
   > new in Phase 3. Until then, "protected" cannot be measured.

3. **Quality gate on all writes**: every created/patched skill must pass a
   deterministic structural gate (description, substantive body, markdown
   structure, no placeholder) before it is accepted.
4. **Judge-gated improvement**: a proposed change is accepted only if an
   LLM-as-judge (or a cheaper proxy) scores it ≥ baseline + margin *and* growth
   is within a bound. Improvement is measured, not assumed.
5. **"Do NOT capture" rules**: never persist env-dependent failures,
   negative claims ("X tool is broken"), transient errors, or unresolved
   failures dressed up as best practice. This is verbatim guidance from
   Hermes's `_SKILL_REVIEW_PROMPT`.
6. **Best-effort, non-blocking**: the whole pipeline runs in the background,
   never competes with the user's task for attention, and never blocks or
   fails the foreground turn.

Throughout this document, "auto-editing" and "the automatic pass" both mean
the review fork that runs under pillars 1-6. The terms "review fork", "review
agent", and "review worker" are used interchangeably for that same restricted
sub-agent. "Jcode-managed" means a skill explicitly created or adopted by the
automatic pass (provenance added in Phase 3); user-authored, endorsed/catalog,
hub-installed, and pinned skills are never jcode-managed.

## Phased plan

### Phase 1: Skill quality gate (deterministic, no LLM)

Port the structural guardrail from `hermes-agent-self-evolution/evolution/core/constraints.py`
(`_check_skill_structure`, `_check_non_empty`) and `tools/skill_linter.py`.

- Add a `SkillQuality` struct in `crates/jcode-base/src/skill.rs`:
  `has_description`, `has_body`, `has_structure` (heading/list), `has_placeholder`,
  `body_chars`. A `passes_gate()` and `issues()` helper.
- Unit-test the gate: empty/stub body, placeholder-only, valid multi-section,
  and the boundary at the 40-char stub threshold.
- Compute in `parse_skill_inner`; warn on load for failing skills; annotate
  them in `skill_manage list`.
- **Keep it advisory**: legacy skills still load. This is a routing/quality
  signal, not an install blocker.

> This is the prerequisite: auto-created skills must be *good*, which requires
> first being able to *recognize* good vs. stub.

### Phase 2: Judged skill improvement (the "self-improving" core)

Mirror Hermes's `background_review` as a **restricted tool-calling sub-agent**
hosted on the existing swarm worker runtime (`jcode-app-core/src/server/
comm_session.rs::spawn_swarm_agent`), not on `MemoryAgent` (which is a
serialized single-task extractor and cannot call `skill_manage`). `MemoryAgent`
stays the tripwire that notices "a review is due", but the review itself runs
on a restricted swarm worker. `jcode-swarm-core` is only the protocol/types
crate for that runtime, not the executor.

> Ordering note: Phase 2's review agent only ever *writes* skills via the
> surface and protection flags from Phase 3. Depend on Phase 3 (or land a
> minimal read-before-write + protected-set gate in Phase 2 first); do not
> ship the reviewer against an unguarded write path. Also, do not *apply* any
> change until the judge gate (Phase 4) exists; until then the reviewer can
> propose a change but not commit it. Writing before the judge exists would
> let unmeasured changes through, defeating pillar 4.

- `MemoryAgent` already watches fresh user turns; when the skill-review count /
  cooldown fires, it enqueues a review onto the swarm runtime instead of doing
  the review itself.
- Trigger: `MemoryAgent` fires (via `update_context_sync_with_dir`) after a
  fresh user turn. Hermes nudges every N iterations (default 10); adopt a
  configurable interval, gated by "did the turn actually use a skill or
  produce signal."
- **Throttle against the memory agent**: both pipelines run their own LLM
  extraction per fresh turn, so the skill review must share a cooldown /
  rate-limit with `MemoryAgent` to honor the best-effort, non-blocking pillar.
  Adopt a minimum gap between reviews and skip while a memory extraction is
  in flight.
- **Respect the live-session KV cache**: a global-skill reload registers a
  cache-invalidation in jcode (`skill.rs::reload_global`: "the skills list in
  the system prompt may have changed"), which busts warm prompt prefixes for
  every session. Batch auto-writes and trigger a single reload in an idle
  window rather than reloading per change, so an autonomous write does not
  repeatedly break foreground prompt caching.
- The review prompt (adapted from `_SKILL_REVIEW_PROMPT`) instructs the model
  to: `skill_patch` the loaded skill that was in play, else patch an existing
  umbrella, else add a `references/`/`templates/`/`scripts/` support file,
  else create a new class-level skill. Enforce "read-before-write". When
  patching an in-play skill, keep the patch at the skill's existing location
  (global vs project-local) rather than moving it, and let the protection
  flags govern whether it is even editable.
- **Fork isolation**: the swarm worker's toolset is `skill_manage`
  (patch/create/write_file) + `memory`. Deny everything else (mirror Hermes's
  "deny everything not whitelisted").
- **Don't persist the user's raw words**: the review reads the conversation to
  learn, but a saved skill codifies *how to do a task*, never the user's live
  content verbatim. Reuse the Phase 5 secret/false-positive-aware redaction on
  this write path too (not just session mining), so a skill never captures
  credentials or sensitive excerpts from the turn it learned from.
- **Escaped-loop + cost bounds**: the review worker gets a hard `max_iterations`
  cap and a per-review token budget so a misbehaving or looping pass cannot run
  forever or spend unbounded tokens. Hermes caps the background review fork at
  16 iterations (`_REVIEW_MAX_ITERATIONS` in `background_review.py`) and puts
  a separate 9999 cap on the day-long curator pass; jcode's per-turn review
  should use a small cap like 16. These bounds are required for the best-effort,
  non-blocking pillar.
- **Protected-skills** filter enforced at the tool boundary (see Phase 3).
  Until Phase 3 ships, the effective protected set is "everything"; the
  reviewer can propose but not apply.

### Phase 3: Skill write/management surface + protections

Expand `skill_manage` from `load/list/reload/reload_all/read` to a full
lifecycle, with the Hermes-style validators (`skill_manager_tool.py`):

- `create`, `patch` (content or targeted old→new), `write_file`, `delete`.
- Validators mirroring `skill_manager_tool.py`: name regex
  `^[a-z0-9][a-z0-9._-]*$`, non-empty content, frontmatter `name`+`description`
  present, and a content size cap (Hermes: 100,000 chars). Note jcode's slash
  parser is stricter (no dot in command names), so the write path should apply
  the broader Hermes-style regex or jcode's rule consistently.
- Advisory linter findings surfaced after create/edit (`skill_linter.py`):
  name-dir mismatch, missing `## When to Use`, dangling `references/` links,
  marketing words in description. Also enforce a **description-length budget**
  so the routing signal survives: jcode clips skill descriptions at
  `SKILL_DESC_MAX_CHARS = 120` in the system prompt (`prompt.rs`), so an
  auto-created skill's `description:` should be a tight one-liner well under
  that. (Hermes uses a 60-char budget; adapt to jcode's constant, not a blind
  copy.)
- **Protection flags**: introduce provenance on `Skill` (endorsed/catalog,
  hub-installed, user-authored, jcode-managed) so the automatic pass can't
  touch the protected set. Also decide **placement** for auto-created skills:
  default to global `~/.jcode/skills/` so the learning is cross-session and
  cross-repo, and let a project-local `.jcode/skills/` placement be a per-session
  opt-in (respecting the existing overlay model). Because globals are
  cross-repo by default, keep repo-specific details out of them: a
  project-local placement is the right home for a skill that only makes sense
  in one workspace, and the "do NOT capture" rule against one-off narratives
  already steers this.
- **Config gate**: add a feature toggle (default off) that controls whether the
  automatic pass may apply writes at all. Follow the existing `enabled`
  config-flag pattern so auto-editing is always opt-in.
- **Approval + undo path** (needed because auto-editing is sensitive): stage
  auto-written skills for review and provide rollback. Mirror Hermes's
  `_apply_skill_write_gate` / `apply_skill_pending` staging and per-write
  rollback, plus an audit ledger. The user can inspect a proposed skill, approve
  or reject it, and revert any applied one.

### Phase 4: Judge + growth bound (measure before accepting)

Port the fitness layer from `fitness.py` and the growth constraint from
`constraints.py`:

- LLM-as-judge scoring on correctness / procedure-following / conciseness with
  textual feedback. The bare "score ≥ baseline" rule is only a floor; apply
  the tightened gate below.
- **Calibrate the judge, don't trust one sample**: a single LLM judge score is
  noisy, so require a margin (e.g. ≥ baseline + ε) across multiple judge
  rollouts before applying, or apply-then-verify on the next turn. Hermes
  `fitness.py` does neither, so this margin is an explicit jcode improvement
  over the naive ≥ baseline comparison.
- Growth cap (Hermes: 20% over baseline) so improvement doesn't bloat.
- Store a small results ledger per skill so jcode can point at "this skill got
  3 judged improvements" as the evidence the loop is working.

### Phase 5: Mine jcode's own sessions (data for the loop)

Port the session-mining idea from `external_importers.py`, but source from
jcode's own `session_search` / embedded history instead of external tools:

- Build (task, expected-behavior) pairs from past sessions for a skill's
  eval set.
- Reuse the **secret-detection** regex set (API keys, tokens, PEM) so jcode
  never *learns* from leaked credentials. Note the tradeoff: these regexes are
  anchored to reduce, not eliminate, false positives (`password=`, `token:`,
  long `sk-` prefixes) and can redact legitimate code or docs. Redact from the
  *learnt* data only, never from the user's real sessions, and keep a manual
  review path for anything the filter drops. Add regression tests for the
  secret patterns so a bundled sample never slips into a learnt skill.

### Phase 6: Skill telemetry + light curator (longer horizon)

- A `.usage.json`-style sidecar (`skill_usage.py`) tracking per-skill activity,
  feeding lifecycle decisions (active → stale → archived) with a `pinned` flag.
- Optionally a background curator to consolidate overlapping skills that the
  self-improvement pass accumulates. jcode has no dedicated idle/maintenance
  scheduler today, so this phase introduces one (e.g. a batched background
  loop like the swarm heartbeat at `server/comm_control.rs`) rather than
  reusing the user-launched `/overnight` mission.

## Integration points in jcode (verified in code)

| Concern | Location |
|---|---|
| Background LLM pipeline host | `crates/jcode-base/src/memory_agent.rs` (`MemoryAgent`, `process_context`, `extract_from_context`, `tokio::spawn(agent.run())`) |
| Review fork sub-agent host | `jcode-app-core/src/server/comm_session.rs` (`spawn_swarm_agent`) + `server/state.rs` (`SwarmRuntime`); `jcode-swarm-core` is the protocol/types crate |
| Skill registry + parse | `crates/jcode-base/src/skill.rs` (`SkillRegistry`, `parse_skill_inner`, `Skill`) |
| Skill tool surface | `crates/jcode-app-core/src/tool/skill.rs` (`SkillTool`) |
| Trigger for review | `crates/jcode-app-core/src/agent/prompting.rs` (`build_memory_prompt_nonblocking_shared` / `update_context_sync_with_dir`) |
| Background event loop (memory/skill reviews) | `crates/jcode-base/src/memory_agent.rs` background loop; a curator cadence needs a new batched loop (`server/comm_control.rs` swarm heartbeat is the pattern; `overnight-core` is a user-launched mission, not a daemon) |
| Session history (eval source) | `crates/jcode-app-core/src/tool/session_search.rs` |

## Guardrails summary

- **Off by default**: the automatic pass applies no writes until the user
  enables the feature toggle.
- Auto-edits restricted to **jcode-managed** skills only (provenance added in
  Phase 3; until then the effective protected set is everything).
- Review fork carries a **restricted toolset** (skill + memory), throttled
  against the memory pipeline, with **max-iteration and token-budget caps**.
- All writes pass the **quality gate** + **judge (≥ baseline + margin)** +
  **growth cap**.
- Auto-writes are **staged for approval with an undo path** (inspect, reject,
  or revert any applied change).
- Auto-writes are **batched into an idle window** to avoid repeatedly busting
  live sessions' warm KV-cache prefixes.
- "Do NOT capture" rules prevent self-imposed refusals and rote failure logs.
- Best-effort: nothing in this plan blocks or slows the foreground turn.

## Out of scope (for now)

- DSPy/GEPA full prompt-evolution loop and Python optimizer infra (the *engine*
  Hermes uses; jcode's version is a `skill_patch` + judge loop instead).
- Training the next model generation (Hermes `batch_runner` trajectory
  compression).
- Cross-user / fleet skill sync (Hermes `skills_sync`).

## Success criteria

1. A skill that is "wrong, missing a step, or outdated" in one session is
   patched and correct in the next.
2. A user correction of style/workflow becomes a reusable skill (staged,
   judge-scored, and approval-pending) without a foreground prompt.
3. Stub/placeholder skills are visible in `skill_manage list` (annotated).
4. Every accepted improvement is judge-scored ≥ baseline + margin and within
   growth bounds.
5. No user-authored skill is ever auto-edited.
6. No foreground turn is slowed or blocked.
7. Nothing is auto-written until the user enables the feature toggle.
8. Auto-created skills land in the intended location (global by default) and
   are visible/usable in the right sessions under the existing overlay model.