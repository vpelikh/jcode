# Postmortem: destructive-command gate bypass routes (#604)

**Summary.** A model ran `rm -rf ~` and it was obeyed immediately, destroying a
user's home directory. The fix was a deterministic blast-radius classifier with
a reflection gate — but automated review then found *three* ways to reach that
same catastrophic delete without the gate firing. Guardrail: `jcode-command-risk`
classifies commands by what they would destroy (not by name), unwraps
wrapper/pipeline indirection, and hard-denies the catastrophic tier with no
justification path.

## What happened

jcode executed `bash` tool calls with no gate of its own. The only check in
`ToolRegistry::execute` was an opt-in external `pre_tool` hook that is off by
default. A model that decided to run `rm -rf ~` was obeyed immediately. That is
issue #604, and a user lost their home directory.

Two things made the first fix insufficient, and review is what proved it:

1. The gate that landed was deliberately **blast-radius-first** so a denylist
   of `rm -rf` could not be beaten by `find -delete`, `shred`, `truncate`, `dd`,
   or `>file`. But that first classifier only looked at the *first* token of a
   segment — so the same destructive verb hidden behind a wrapper or a pipeline
   was invisible and classified `Safe`.
2. The reflection gate (stage 2) turns a `Confirm` into a prompt asking the
   model to justify against the user's actual request, and an absolute-deny tier
   blocks the catastrophic targets (`/`, `$HOME`, credential stores, device
   nodes) with no unlock. The scope was mostly right, but the *routes* into the
   targets were wider than the classifier saw.

## Root-cause chain

- `ToolRegistry::execute` allowed an un-gated model command through (#604).
- The stage-1 classifier inspected only `tokens.first()`, so `sudo rm -rf ~`,
  `env rm -rf $HOME`, and `nice -n 10 rm -rf ~` all classified `Safe` — the
  destructive verb was never examined (wrapper commands).
- Pipelines hid operands: `find ~ -type f | xargs rm -rf` deletes home contents,
  but neither segment shows it, because `xargs` takes its operands from stdin at
  runtime (piped deletes).
- Some conditional flags / recursive operands that implied destruction were not
  surfaced by the first scan (the third bypass class closed by review).
- `apply_patch` deleted by absolute path independently, and exact-match
  credential protection missed files *inside* a protected store (separate
  postmortem `protected-path-deletion`).

## Safety nets that failed (in order)

1. **No gate at all** — the pre-tool hook was opt-in and off by default.
2. **Name-based / token-first recall** — a denylist or first-token scan misses
   wrappers, pipelines, and alternates.
3. **First classifier's blast-radius model was sound in intent** but its
   parser had blind spots; these were not exercised until automated review.
4. **Review** — this is the net that *caught* the class: it reproduced each
   bypass before fixing, then pinned the fix with tests.

## Guardrails added

- **`crates/jcode-command-risk`** — stage 1 `assess()` classifies by blast
  radius, not command name. `wrapper_flag_takes_value` and recursive wrapper
  unwrapping make `sudo`, `env`, `timeout`, `xargs`, `sh`, etc. resolve to the
  program underneath. Segments receiving a pipe escalate (their operands cannot
  be enumerated statically). Conditional destructive flags (`find -delete`,
  `git clean`, `chmod -R`) and truncating redirects (`>file`) are targets too.
- **Reflection gate** (`src/gate.rs`) — stage 2 is *not* a second model. A
  `Confirm` verdict returns a structured prompt that forces the generating model
  to name which user request the delete serves; a blind identical retry fails
  (`Justification::is_substantive` rejects empty affirmations). The
  `Catastrophic` tier is `Deny` with no unlock.
- **Hard absolute deny** for `HOME`, `/`, credential stores, device nodes —
  a path-based deny that does not depend on parsing the command correctly.
- **Wiring `before` all escape hatches** — the gate sits in `bash.rs` before the
  `run_in_background` early return (see the background-dispatch postmortem).

## Why review, not the first gate, caught it

The first gate was correct in spirit and wrong in reach. Static command parsing
has unbounded edge cases (wrappers, pipes, nested shells), and the cost model
favors recall over precision. The lesson is not "the gate was bad" — it is that
a safety gate with a recall bias must be **adversarially reviewed** against the
exact failure it exists to stop, and each escape-route class needs a pinned test.
The three commits that landed after review are the durable record of that.

## Guardrail home

- Classifier + gate: `crates/jcode-command-risk/src/lib.rs`,
  `src/gate.rs`, `src/paths.rs`, `src/tokenize.rs` (with module-local test
  suites `assess_tests.rs`, `gate_tests.rs`, `paths_tests.rs`).
- Bash wiring: `crates/jcode-app-core/src/tool/bash_destructive_gate.rs`.

## Related

- `changelog/v0.61.0.json` — release note: "Destructive shell commands ... now
  pause for an explicit reflection step before running, closing several bypass
  routes found in review."
- `docs/research/deepseek-harness-takeaways.md` takeaway #8 (sandbox fail-closed,
  strictly-wider escalation) — the next step beyond classification toward actual
  confinement.