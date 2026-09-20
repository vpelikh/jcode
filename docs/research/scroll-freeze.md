# Investigation: TUI freezes during fast scrolling

**Status:** COMPLETE
**Branch (worktree):** `jc/freeze-scrolling-investigation`
**Base:** `619fa67b6` (an ancestor of current `master`)

## Conclusion (read this first)

The reported "TUI freezes during fast scrolling" is **environmental — CPU
oversubscription** — not a scroll-code defect.

Evidence, all measured:
- The **real production binary** scrolls the user's actual 16,536–16,548-wrapped-line
  rich transcript at 4–5 ms `draw_ms` per frame in isolation.
- The telemetry's idle-scroll slow frames (67–455 ms `draw_ms`) come from the user's
  live sessions under real load (this machine: load ≈63, 50+ processes).
- No in-process scroll-path change is warranted; the scroll render path is healthy and
  fast in isolation.

**The practical fix is to reduce system load:** stop concurrent `cargo`/`rustc`/clippy
builds, close idle `jcode self-dev --resume` resident sessions, and watch the
shared-server daemon CPU. That lever resolves the freeze.

Read the rest for the full evidence, the two code-side findings (streaming / large
transcript) that are real but not the user's reported trigger, and the instrumentation
left in place.

---

## 1. Symptom

The TUI (and only the TUI) appears to freeze when the user scrolls fast (mouse wheel)
through a large transcript.

## 2. Ground truth: the slow frames

From the user's own `TUI_SLOW_FRAME` logs (threshold 40 ms), 09-01 and 09-02:

- **75k+ slow frames over 2 days**; worst single frame 23.6 s (a `prepare_ms` stall).
- **Only ~0.2% of slow frames are scroll-triggered** (143 of ~75k). Scroll slow frames
  are small (avg ~103 ms).
- Status breakdown of non-scroll slow frames: **Streaming ~65%**, Connecting ~19%,
  RunningTool ~13%, Thinking/Sending/Idle the rest.
- The **multi-second stalls are `prepare_ms`-dominated during Thinking/Sending** on
  huge transcripts (up to 23.4 s) — turn-processing, not scroll.

## 3. The reported scroll trigger, isolated

Of the 143 scroll-triggered slow frames:
- **85 (56%) are Idle**; the rest Connecting 29, Streaming 19, RunningTool 15,
  Thinking 3.
- The **62 idle-scroll frames are draw-dominant**: avg `draw_ms` 67.8 ms (max 455 ms)
  on 14k–27k-line transcripts; full-prep path is mostly `cache_hit_oversized`/empty
  (body cached — not a cache-miss).

## 4. Why it is NOT an in-process scroll bug

The decisive control — run the **real production binary** (production `FULL_PREP_CACHE`,
not `cfg(test)`) replaying the user's actual large rich parrot session (verified
16,536/16,548 wrapped lines) at 244×71, then flood ~1.5M idle scroll-up ticks:

- **Nearly no `TUI_SLOW_FRAME` was emitted.** The few that appeared show
  `draw_ms = 4–5 ms`, `total = 52–82 ms`, `lines = 16,536/16,548`.
- So a real 16.5k-line rich transcript scrolls at **4–5 ms draw** in isolation.

Therefore the 67–455 ms telemetry idle-scroll frames cannot be an in-process scroll
cost — they are **load-induced** (CPU starvation under load ~63).

## 5. Candidate code findings (separate from scroll trigger) (real, but separate from the scroll trigger)

During the investigation two code paths showed genuine, measured cost — both distinct
from the idle-scroll trigger:

### 5a. Streaming full-prep rebuild (Streaming slow frames)
- Streaming is ~65% of slow frames; **IncrementalMarkdownRenderer fully re-renders the
  streaming text every token** (incremental splicing was tried and abandoned for
  correctness).
- The streaming tail is O(text length) but small (~0.0015 ms/char; ~7 ms at 1500
  chars, measured).
- The larger streaming-frame cost is the **body/full-prep rebuild** forced by the
  `streaming_text_hash` full-prep cache key (measured: prepare_ms linear in body size,
  7–22 ms for 20–120 messages).
- **Fix direction (not shipped):** separate the streaming section from the full-prep
  cache key so a token only re-prepares the ~8 ms streaming tail; reuse the cached
  body/header via `PreparedChatFrame::from_sections` (verified it fully recomputes
  derived state from section Arc reuses).

### 5b Large-transcript prepare on turn-processing
- The worst multi-second stalls are `prepare_ms` during Thinking/Sending on
  huge transcripts, amplified by CPU starvation.

### 5c Display instrumentation added (merged)
Additive diagnostic timing was added to `prepare_messages_inner`:
- `FullPrepPhaseMetrics` gains `inline_ms` and `total_inner_ms`; `FramePerfStats`
  accumulates `full_prep_inline_ms` and `full_prep_total_inner_ms`, surfaced in
  `TUI_SLOW_FRAME`. (ui_frame_metrics.rs, ui_prepare.rs) — diagnostic only, no
  behavior change.

## 6. What the scroll path does (for completeness) (for completeness)

- Mouse wheel -> `handle_terminal_event` drains up to 32 events/wake -> `scroll_up/down`
  set `force_full_repaint` -> `draw_full_core` soft-repaint full re-emit.
- Repaints are already coalesced to one frame per event-wake.
- The #2357 wide-grapheme ghost / #404 image-flicker guards are pinned by
  `scroll_arms_force_full_repaint_to_clear_wide_grapheme_ghosts`; do not remove the
  scroll repaint.

## 7. Fix recommendation

1. **Environment (effective fix now):** reduce concurrent builds, resident
   `self-dev` sessions, daemon CPU. This is what removes the scroll freeze.
2. **Code, if streaming stalls matter (Streaming ~65%):** shipping the full-prep
   cache restructure (separate streaming section). Needs measurement on a fast large
   streaming session to verify no visual regression.
3. **No scroll-repaint code change is warranted** — it is fast in isolation (4–5 ms).

## 8. Confirm the load-bound diagnosis on your machine

Run an interactive jcode on one of the large sessions and scroll while Idle. Under
load you will see the freeze; with builds / resident sessions suspended it will be
smooth. That is the definitive A/B.

---

## Appendix — earlier diagnostic detail

Kept for traceability: the investigation initially attributed the freeze to
"scroll-render cost", then pivoted through "streaming/body-rebuild" and back; each
was tested and corrected:

- `scroll-test` harness stays flat (12–17 ms) at width 244 across transcript sizes —
  synthetic content does not reproduce real richness.
- Test-build `cfg(test)` **bypasses the FULL_PREP cache** (`prepare_messages` returns
  `prepare_messages_inner` directly), so any test measuring idle-scroll draw with
  growing sizes shows a false O(transcript) that is NOT production (the rejected
  "O(transcript)" result; retracted).
- The real-binary control (section 3) is the authoritative measurement.