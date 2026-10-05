#!/usr/bin/env bash
# Integration tests for the Cargo build gate's release/trap behavior, driven
# through the real dev_cargo.sh wrapper with a fake `cargo` that sleeps. This
# covers failure modes that unit-testing the gate block in isolation cannot:
#
#   - the slot is released when cargo exits normally or non-zero;
#   - a SIGTERM to the wrapper does NOT free the slot while cargo still runs (the
#     wrapper defers the signal; releasing early would let another build start and
#     exceed the bound). The slot is released once cargo exits.
#   - a SIGKILLed wrapper leaves a stale slot that the next build reclaims;
#   - a slot is actually held for the duration of the build.
#
# See scripts/test_dev_cargo_gate.sh for the semaphore semantics themselves.
set -uo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/bin" "$root/gate"

# Fake `cargo`: sleep, then exit with $JCODE_FAKE_CARGO_EXIT (default 0).
cat > "$root/bin/cargo" <<'EOF'
#!/usr/bin/env bash
sleep "${JCODE_FAKE_CARGO_SLEEP:-2}"
exit "${JCODE_FAKE_CARGO_EXIT:-0}"
EOF
chmod +x "$root/bin/cargo"

# Put the fake cargo first and bypass any shell-level cargo shim.
export PATH="$root/bin:$PATH"
export JCODE_IN_DEV_CARGO=1
export JCODE_CARGO_GATE_DIR="$root/gate"
export JCODE_CARGO_GATE_SLOTS=1
export JCODE_RUST_ACTION_LOG=0
# Do not let a shared target dir make the gate take its "capacity 1 anyway" path
# for the wrong reason; slots are forced to 1 explicitly above.
unset JCODE_SHARED_TARGET_DIR CARGO_TARGET_DIR

slots_dir="$root/gate/slots"
dev="scripts/dev_cargo.sh"

pass=0
fail=0
ok() { echo "ok: $1"; pass=$((pass + 1)); }
bad() { echo "FAIL: $1"; fail=$((fail + 1)); }
count_slots() { ls -1 "$slots_dir" 2>/dev/null | wc -l | tr -d ' '; }

# 1. A successful run leaves no slot behind.
JCODE_FAKE_CARGO_SLEEP=1 JCODE_FAKE_CARGO_EXIT=0 "$dev" check -p jcode-fuzzy >/dev/null 2>&1
[[ "$(count_slots)" == "0" ]] && ok "slot released after success" \
  || bad "slot leaked after success: $(count_slots)"

# 2. A failed run leaves no slot behind and propagates the exit code.
JCODE_FAKE_CARGO_SLEEP=1 JCODE_FAKE_CARGO_EXIT=3 "$dev" check -p jcode-fuzzy >/dev/null 2>&1
rc=$?
[[ "$rc" == "3" ]] && ok "cargo exit code propagated ($rc)" || bad "exit code not propagated: $rc"
[[ "$(count_slots)" == "0" ]] && ok "slot released after failure" \
  || bad "slot leaked after failure: $(count_slots)"

# 3. The slot is held for the duration of a real run.
JCODE_FAKE_CARGO_SLEEP=6 "$dev" check -p jcode-fuzzy >/dev/null 2>&1 &
runpid=$!
sleep 2
held=$(count_slots)
[[ "$held" == "1" ]] && ok "slot held while cargo runs" || bad "slot not held during run: $held"
wait "$runpid"
[[ "$(count_slots)" == "0" ]] && ok "slot released after long run" \
  || bad "slot leaked after long run: $(count_slots)"

# 4. SIGTERM mid-build must NOT free the slot while cargo still runs, or another
#    build could start and exceed the bound. The wrapper defers the signal until
#    cargo exits, then releases the slot.
JCODE_FAKE_CARGO_SLEEP=7 "$dev" check -p jcode-fuzzy >/dev/null 2>&1 &
runpid=$!
sleep 2
kill -TERM "$runpid" 2>/dev/null
sleep 2
held_after_term=$(count_slots)
[[ "$held_after_term" == "1" ]] && ok "slot still held after SIGTERM while cargo runs" \
  || bad "slot freed by SIGTERM while cargo ran: $held_after_term"
wait "$runpid" 2>/dev/null
released=no
for _ in $(seq 1 25); do
  [[ "$(count_slots)" == "0" ]] && { released=yes; break; }
  sleep 0.2
done
[[ "$released" == "yes" ]] && ok "slot released after cargo exits following SIGTERM" \
  || bad "slot not released after cargo exit: $(count_slots)"

# 5. SIGKILL leaves a stale slot that the next build reclaims and cleans up.
JCODE_FAKE_CARGO_SLEEP=30 "$dev" check -p jcode-fuzzy >/dev/null 2>&1 &
runpid=$!
sleep 2
kill -9 "$runpid" 2>/dev/null
wait "$runpid" 2>/dev/null
stale=$(count_slots)
[[ "$stale" == "1" ]] && ok "SIGKILL leaves a stale slot ($stale)" \
  || bad "expected 1 stale slot, got $stale"
if timeout 20 env JCODE_FAKE_CARGO_SLEEP=1 "$dev" check -p jcode-fuzzy >/dev/null 2>&1; then
  ok "next build reclaims the stale slot and runs"
else
  bad "next build did not reclaim the stale slot"
fi
[[ "$(count_slots)" == "0" ]] && ok "reclaimed slot cleaned up" \
  || bad "slots left after reclaim: $(count_slots)"

# 6. An unwritable gate directory must not block the build. The gate is a safety
# bound, not a correctness requirement; failing closed on a read-only dir (or a
# full disk) would stop every build, which is worse than briefly oversubscribing.
ro="$root/readonly-gate"
mkdir -p "$ro"
chmod 500 "$ro"
out=$(env JCODE_CARGO_GATE_DIR="$ro" JCODE_FAKE_CARGO_SLEEP=1 JCODE_FAKE_CARGO_EXIT=0 \
  "$dev" check -p jcode-fuzzy 2>&1)
rc=$?
chmod 700 "$ro"
[[ "$rc" == "0" ]] && ok "build runs despite unwritable gate dir" \
  || bad "unwritable gate dir blocked the build (rc=$rc): $(tail -1 <<<"$out")"

# 7. The explicit slot override bypasses memory sizing entirely (it is the
#    rustc/memory-independent path), so --print-setup must report exactly that.
#    This checks the override is plumbed through to the reported capacity on the
#    real script path regardless of the host's memory/core count.
cap_line=$(env -u JCODE_SHARED_TARGET_DIR -u CARGO_TARGET_DIR \
  JCODE_CARGO_GATE_SLOTS=3 JCODE_CARGO_GATE_DIR="$root/gate" \
  bash scripts/dev_cargo.sh --print-setup 2>/dev/null | grep '^cargo_gate_capacity=')
[[ "$cap_line" == "cargo_gate_capacity=3" ]] \
  && ok "print-setup capacity honours JCODE_CARGO_GATE_SLOTS override" \
  || bad "expected cargo_gate_capacity=3 from override, got: ${cap_line:-<none>}"

echo "---"
echo "gate integration: pass=$pass fail=$fail"
[[ "$fail" == "0" ]]
