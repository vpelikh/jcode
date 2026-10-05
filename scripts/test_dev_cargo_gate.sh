#!/usr/bin/env bash
# Unit tests for the bounded Cargo build gate in scripts/dev_cargo.sh.
#
# The gate bounds how many compile-capable Cargo actions run at once. It must be
# a counting semaphore, not a mutex: a mutex serializes even build *sets* that
# could run in parallel, which is strictly worse than what Cargo already does
# (Cargo takes an exclusive lock on a shared build directory). On macOS there is
# no `flock` CLI, so the gate is implemented with atomically-created slot
# directories; these tests extract that block via its sentinels and exercise it.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
wrapper="$repo_root/scripts/dev_cargo.sh"

if ! grep -q '^# jcode: cargo-gate-section' "$wrapper" \
  || ! grep -q '^# jcode: end-cargo-gate-section' "$wrapper"; then
  echo "FAIL: gate section sentinels missing from $wrapper" >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

awk '
  /^# jcode: cargo-gate-section/ { capture = 1; next }
  /^# jcode: end-cargo-gate-section/ { capture = 0; next }
  capture { print }
' "$wrapper" > "$tmp/gate.sh"

# shellcheck disable=SC1090
source "$tmp/gate.sh"

log() { printf 'gate-test: %s\n' "$*" >&2; }

# The gate block calls these host-sizing helpers (defined elsewhere in
# dev_cargo.sh, so not in the extracted section). The tests that use them all set
# an explicit JCODE_CARGO_GATE_SLOTS, but the invalid-override case deliberately
# falls through to the derived capacity, so provide deterministic stand-ins.
cpu_count() { echo "${GATE_TEST_CPUS:-4}"; }
available_memory_kib() { echo "$(( ${GATE_TEST_MEM_MIB:-8192} * 1024 ))"; }

export JCODE_CARGO_GATE_DIR="$tmp/gate"

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

# Reset shell state only. Deliberately does not touch slot directories: a
# competitor subshell must not delete the slots the parent is holding.
reset_gate_vars() {
  unset JCODE_CARGO_GATE_HELD JCODE_CARGO_GATE
  cargo_gate_status=""
  cargo_gate_slot_dir=""
}

# Reset shell state and clear all slots; use between independent phases.
reset_gate_all() {
  reset_gate_vars
  rm -rf "$JCODE_CARGO_GATE_DIR/slots"
}

# --- Action predicate ------------------------------------------------------
# dev_cargo.sh is a shared entry point: scripts and the Rust bash/selfdev paths
# call it for build/check/clippy/run too, so this decides what can oversubscribe.
for action in build check clippy test bench run rustc rustdoc; do
  cargo_argv=("$action")
  cargo_action_needs_gate || fail "$action should be gated"
done
for action in fmt metadata fetch update tree; do
  cargo_argv=("$action")
  cargo_action_needs_gate && fail "$action should not be gated"
done
echo "ok: only compile-capable actions are gated"

# --- Capacity 1: behaves like a lock --------------------------------------
export JCODE_CARGO_GATE_SLOTS=1
cargo_argv=(test)
reset_gate_all
acquire_cargo_gate
[[ "$cargo_gate_status" == "acquired" ]] || fail "first acquire status=$cargo_gate_status"
[[ -d "$cargo_gate_slot_dir" ]] || fail "slot dir not created"
echo "ok: capacity 1 first acquire wins"

(
  reset_gate_vars
  acquire_cargo_gate
  echo "child acquired slot=${cargo_gate_slot_id} waited_ms=$cargo_gate_wait_ms" > "$tmp/child.out"
) &
child=$!
sleep 3
kill -0 "$child" 2>/dev/null || fail "competitor did not block at capacity 1"
echo "ok: capacity 1 competitor blocked while held"
release_cargo_gate
wait "$child"
grep -qE "child acquired slot=0 waited_ms=[1-9][0-9]*" "$tmp/child.out" \
  || fail "competitor recorded no wait: $(cat "$tmp/child.out")"
echo "ok: capacity 1 competitor acquired after release"

# --- Capacity 2: two builds run in parallel, third waits -------------------
export JCODE_CARGO_GATE_SLOTS=2
reset_gate_all
acquire_cargo_gate
[[ "$cargo_gate_slot_id" == "0" ]] || fail "expected slot 0, got $cargo_gate_slot_id"
GATE_SLOT_DIR_A="$cargo_gate_slot_dir"
# Emulate a second, independent process taking the second slot: clear the status
# and the inherited flag, but deliberately leave the first slot directory held.
cargo_gate_status=""
unset JCODE_CARGO_GATE_HELD
acquire_cargo_gate
[[ "$cargo_gate_slot_id" == "1" ]] || fail "expected slot 1, got $cargo_gate_slot_id"
echo "ok: capacity 2 admits two concurrent builds"

(
  reset_gate_vars
  acquire_cargo_gate
  echo "third acquired slot=${cargo_gate_slot_id}" > "$tmp/third.out"
) &
third=$!
sleep 3
kill -0 "$third" 2>/dev/null || fail "third build did not wait when both slots were taken"
echo "ok: capacity 2 blocks the third build"
rm -rf "$GATE_SLOT_DIR_A" "$cargo_gate_slot_dir" # release both held slots
wait "$third"
grep -q "third acquired slot=" "$tmp/third.out" || fail "third build never acquired"
echo "ok: capacity 2 third build ran after a slot freed"

# --- Stale slot reclaim ----------------------------------------------------
reset_gate_all
slots_dir="$JCODE_CARGO_GATE_DIR/slots"
rm -rf "$slots_dir"
mkdir -p "$slots_dir/slot-0"
printf '999999\n' > "$slots_dir/slot-0/owner"
acquire_cargo_gate
[[ "$cargo_gate_status" == "acquired" ]] || fail "stale slot not reclaimed"
echo "ok: stale slot reclaimed"

# --- Race: an owner-less slot is not reclaimed while fresh ------------------
# Between `mkdir` (which publishes a slot) and writing the owner file there is a
# window where the slot exists with no owner. Reclaiming it then would let two
# builds share one slot and break the bound. A fresh owner-less slot must be
# treated as live.
reset_gate_all
slots_dir="$JCODE_CARGO_GATE_DIR/slots"
mkdir -p "$slots_dir/slot-0"
live=$(cargo_gate_prune_stale_slots)
[[ "$live" == "1" ]] || fail "fresh owner-less slot was reclaimed (live=$live)"
[[ -d "$slots_dir/slot-0" ]] || fail "fresh owner-less slot dir was deleted"
echo "ok: fresh owner-less slot is not reclaimed"

# --- An owner-less slot older than the grace period is reclaimed ------------
JCODE_CARGO_GATE_STALE_GRACE=30
export JCODE_CARGO_GATE_STALE_GRACE
touch -t 202001010000 "$slots_dir/slot-0"
live=$(cargo_gate_prune_stale_slots)
[[ "$live" == "0" ]] || fail "old owner-less slot not reclaimed (live=$live)"
[[ -d "$slots_dir/slot-0" ]] && fail "old owner-less slot dir survived"
unset JCODE_CARGO_GATE_STALE_GRACE
echo "ok: owner-less slot older than grace is reclaimed"

# --- mtime helper is OS-deterministic --------------------------------------
# It must return a numeric epoch for a real path; a wrong value would corrupt the
# grace-period logic above.
probe_dir="$tmp/mtime-probe"
mkdir -p "$probe_dir"
got=$(cargo_gate_path_mtime "$probe_dir")
[[ "$got" =~ ^[0-9]+$ ]] || fail "cargo_gate_path_mtime did not return a numeric epoch: '$got'"
now=$(date +%s)
(( got <= now + 2 && got >= now - 60 )) || fail "cargo_gate_path_mtime implausible: $got vs $now"
echo "ok: cargo_gate_path_mtime returns a valid epoch for a real path"

# --- PID reuse does not keep a leaked slot alive ---------------------------
# A crashed build can leak a slot whose recorded pid is later reused by an
# unrelated process. Comparing the owner's start time catches that. Guarded so a
# platform where ps -o lstart= is unusable still runs the rest of the suite.
reset_gate_all
slots_dir="$JCODE_CARGO_GATE_DIR/slots"
mkdir -p "$slots_dir/slot-0"
printf '%s\n' "$$" > "$slots_dir/slot-0/owner" # a live pid: this very shell
if start_of_self=$(cargo_gate_proc_start "$$") && [[ -n "$start_of_self" ]]; then
  # Owned by a live pid with a matching start time -> live.
  printf '%s\n' "$start_of_self" > "$slots_dir/slot-0/owner_start"
  live=$(cargo_gate_prune_stale_slots)
  [[ "$live" == "1" ]] || fail "matching start time treated as stale (live=$live)"
  # Same live pid but a mismatched start time -> pid was reused -> stale.
  touch -t 202001010000 "$slots_dir/slot-0"
  printf '%s\n' "Mon Jan  1 00:00:00 2001" > "$slots_dir/slot-0/owner_start"
  live=$(cargo_gate_prune_stale_slots)
  [[ "$live" == "0" ]] || fail "pid reuse not detected (live=$live)"
  [[ -d "$slots_dir/slot-0" ]] && fail "reused-pid slot not reclaimed"
  echo "ok: pid reuse detected via start time"
else
  echo "ok: pid-reuse check skipped (ps -o lstart= unavailable)"
fi

# --- Release does not delete a slot owned by another process ----------------
reset_gate_all
slots_dir="$JCODE_CARGO_GATE_DIR/slots"
mkdir -p "$slots_dir/slot-0"
printf '999998\n' > "$slots_dir/slot-0/owner" # a different, likely-dead PID
cargo_gate_slot_dir="$slots_dir/slot-0"
cargo_gate_status="acquired" # pretend we hold it
release_cargo_gate
[[ -d "$slots_dir/slot-0" ]] || fail "release deleted a slot owned by another process"
echo "ok: release leaves another process's slot alone"

# --- Release removes the slot ---------------------------------------------
reset_gate_all
JCODE_CARGO_GATE_SLOTS=1
export JCODE_CARGO_GATE_SLOTS
acquire_cargo_gate
held_slot="$cargo_gate_slot_dir"
[[ -d "$held_slot" ]] || fail "acquire did not create a slot"
release_cargo_gate
[[ -d "$held_slot" ]] && fail "release left the slot dir behind"
unset JCODE_CARGO_GATE_SLOTS
echo "ok: release removes slot"

# --- JCODE_CARGO_GATE=off --------------------------------------------------
reset_gate_all
JCODE_CARGO_GATE=off
export JCODE_CARGO_GATE
acquire_cargo_gate
[[ "$cargo_gate_status" == "disabled" ]] || fail "JCODE_CARGO_GATE=off not honored"
echo "ok: JCODE_CARGO_GATE=off disables the gate"

# --- Nested inheritance ----------------------------------------------------
reset_gate_all
JCODE_CARGO_GATE_HELD=1
export JCODE_CARGO_GATE_HELD
cargo_argv=(test)
acquire_cargo_gate
[[ "$cargo_gate_status" == "inherited" ]] || fail "nested call did not inherit the gate"
[[ -d "$JCODE_CARGO_GATE_DIR/slots/slot-0" ]] && fail "nested call took a slot"
unset JCODE_CARGO_GATE_HELD
echo "ok: nested call inherits without taking a slot"

# --- Invalid slot override falls back to a sane capacity -------------------
# A non-numeric or zero override must not produce a zero-slot gate (which would
# deadlock every build); it should fall back to the derived capacity.
for bad in "abc" "0" "-1" ""; do
  reset_gate_all
  JCODE_CARGO_GATE_SLOTS="$bad"
  export JCODE_CARGO_GATE_SLOTS
  acquire_cargo_gate
  [[ "$cargo_gate_status" == "acquired" ]] || fail "override '$bad' did not fall back"
  (( cargo_gate_capacity >= 1 )) || fail "override '$bad' produced capacity $cargo_gate_capacity"
  release_cargo_gate
done
unset JCODE_CARGO_GATE_SLOTS
echo "ok: invalid slot overrides fall back to capacity >= 1"

# --- Higher capacity admits that many distinct slots -----------------------
reset_gate_all
JCODE_CARGO_GATE_SLOTS=3
export JCODE_CARGO_GATE_SLOTS
declare -a seen=()
for _ in 1 2 3; do
  cargo_gate_status=""
  unset JCODE_CARGO_GATE_HELD
  acquire_cargo_gate
  seen+=("$cargo_gate_slot_id")
done
# All three slots must be distinct.
unique=$(printf '%s\n' "${seen[@]}" | sort -u | wc -l | tr -d ' ')
[[ "$unique" == "3" ]] || fail "capacity 3 gave non-distinct slots: ${seen[*]}"
# A fourth acquisition must block; verify it cannot take a slot immediately.
cargo_gate_status=""
unset JCODE_CARGO_GATE_HELD
if cargo_gate_try_slot; then fail "4th build took a slot with capacity 3"; fi
rm -rf "$JCODE_CARGO_GATE_DIR/slots"
unset JCODE_CARGO_GATE_SLOTS
echo "ok: capacity 3 admits three distinct slots and blocks the fourth"

# --- Derived capacity bounds total rustc, not just invocations --------------
# With 4 cpus and CARGO_BUILD_JOBS=4, capacity must be 1 so at most one build
# (4 rustc) runs; a naive cap of 2 would allow 8 rustc on 4 cores.
reset_gate_all
unset JCODE_CARGO_GATE_SLOTS
export GATE_TEST_CPUS=4 GATE_TEST_MEM_MIB=100000 CARGO_BUILD_JOBS=4
unset JCODE_SHARED_TARGET_DIR
acquire_cargo_gate
[[ "$cargo_gate_capacity" == "1" ]] || fail "expected capacity 1 (4 jobs x 1 <= 4 cores), got $cargo_gate_capacity"
release_cargo_gate
# More cores than jobs -> capacity scales up to the rustc budget.
export GATE_TEST_CPUS=8 CARGO_BUILD_JOBS=2
reset_gate_all
acquire_cargo_gate
[[ "$cargo_gate_capacity" == "4" ]] || fail "expected capacity 4 (8 cores / 2 jobs), got $cargo_gate_capacity"
release_cargo_gate
# Oversubscription knob raises the rustc budget.
export GATE_TEST_CPUS=4 CARGO_BUILD_JOBS=4 JCODE_CARGO_GATE_RUSTC_OVERSUB=2
reset_gate_all
acquire_cargo_gate
[[ "$cargo_gate_capacity" == "2" ]] || fail "expected capacity 2 with oversub=2, got $cargo_gate_capacity"
release_cargo_gate
unset GATE_TEST_CPUS GATE_TEST_MEM_MIB CARGO_BUILD_JOBS JCODE_CARGO_GATE_RUSTC_OVERSUB
echo "ok: derived capacity bounds total rustc (capacity*jobs <= cores)"

# --- Memory cap interacts correctly with the rustc cap ---------------------
# capacity = min(by_mem, by_rustc). Low memory must win over a large rustc budget.
reset_gate_all
unset JCODE_CARGO_GATE_SLOTS JCODE_SHARED_TARGET_DIR
export GATE_TEST_CPUS=4 GATE_TEST_MEM_MIB=2048 CARGO_BUILD_JOBS=4
acquire_cargo_gate
[[ "$cargo_gate_capacity" == "1" ]] || fail "low-mem capacity should be 1, got $cargo_gate_capacity"
release_cargo_gate
# Plenty of memory, few jobs: rustc cap governs.
export GATE_TEST_CPUS=4 GATE_TEST_MEM_MIB=100000 CARGO_BUILD_JOBS=1
reset_gate_all
acquire_cargo_gate
[[ "$cargo_gate_capacity" == "4" ]] || fail "high-mem/1-job capacity should be 4, got $cargo_gate_capacity"
release_cargo_gate
unset GATE_TEST_CPUS GATE_TEST_MEM_MIB CARGO_BUILD_JOBS
echo "ok: capacity is min(by_mem, by_rustc)"

# --- Real invocation order: jobs are selected before the gate --------------
# The gate computes capacity from CARGO_BUILD_JOBS, which select_build_jobs sets.
# If select_build_jobs ran after acquire_cargo_gate, per-worktree capacity would
# compute off an unset value and collapse to 1. Assert the call order in the
# wrapper's execution tail.
jobs_line=$(grep -n '^select_build_jobs$' "$wrapper" | tail -1 | cut -d: -f1)
gate_line=$(grep -n '^acquire_cargo_gate$' "$wrapper" | tail -1 | cut -d: -f1)
[[ -n "$jobs_line" && -n "$gate_line" ]] || fail "could not locate gate/jobs call sites"
(( jobs_line < gate_line )) \
  || fail "select_build_jobs (line $jobs_line) must run before acquire_cargo_gate (line $gate_line)"
echo "ok: select_build_jobs runs before acquire_cargo_gate (capacity sees jobs)"

# --- Cross-user gate dir override ------------------------------------------
reset_gate_all
unset JCODE_CARGO_GATE_DIR
export JCODE_CARGO_GATE_DIR_HOST="$tmp/shared-gate"
acquire_cargo_gate
[[ "$cargo_gate_dir" == "$tmp/shared-gate" ]] || fail "host gate dir override ignored: $cargo_gate_dir"
release_cargo_gate
unset JCODE_CARGO_GATE_DIR_HOST
export JCODE_CARGO_GATE_DIR="$tmp/gate"
echo "ok: JCODE_CARGO_GATE_DIR_HOST selects a shared gate dir"

echo "all dev_cargo gate tests passed"
