#!/usr/bin/env bash
# Acceptance test for the Cargo build gate: under real concurrent launches, the
# gate must hold its bound. N builds are launched through the real wrapper with a
# slow fake `cargo`, and the number of cargo processes running at once is
# sampled; the observed maximum must equal the capacity, never N.
#
# This is the end-to-end property the regression needed: unbounded concurrency
# (8 builds, load 95 on 12 cores) is exactly what made test runs crawl.
set -uo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/bin" "$root/gate"

cat > "$root/bin/cargo" <<'EOF'
#!/usr/bin/env bash
echo "start $$" >> "$JCODE_ACCEPT_LOG"
sleep "${JCODE_FAKE_CARGO_SLEEP:-3}"
echo "end $$" >> "$JCODE_ACCEPT_LOG"
exit 0
EOF
chmod +x "$root/bin/cargo"

export PATH="$root/bin:$PATH"
export JCODE_IN_DEV_CARGO=1
export JCODE_CARGO_GATE_DIR="$root/gate"
export JCODE_RUST_ACTION_LOG=0
export JCODE_FAKE_CARGO_SLEEP=3
export JCODE_ACCEPT_LOG="$root/events.log"
unset JCODE_SHARED_TARGET_DIR CARGO_TARGET_DIR

run_case() {
  local n="$1" capacity="$2"
  : > "$JCODE_ACCEPT_LOG"
  export JCODE_CARGO_GATE_SLOTS="$capacity"

  local pids=() i
  for ((i = 0; i < n; i++)); do
    scripts/dev_cargo.sh check -p jcode-fuzzy >/dev/null 2>&1 &
    pids+=("$!")
  done

  local max=0 starts=0 ends=0 running
  local samples=$(( (n + 2) * 20 ))
  for ((i = 0; i < samples; i++)); do
    starts=$(grep -c "^start " "$JCODE_ACCEPT_LOG" 2>/dev/null || true); starts=${starts:-0}
    ends=$(grep -c "^end " "$JCODE_ACCEPT_LOG" 2>/dev/null || true); ends=${ends:-0}
    running=$(( starts - ends )); (( running < 0 )) && running=0
    (( running > max )) && max=$running
    (( starts >= n && ends >= n )) && break
    sleep 0.25
  done
  wait "${pids[@]}" 2>/dev/null
  starts=$(grep -c "^start " "$JCODE_ACCEPT_LOG" 2>/dev/null || true); starts=${starts:-0}
  ends=$(grep -c "^end " "$JCODE_ACCEPT_LOG" 2>/dev/null || true); ends=${ends:-0}

  if (( starts == n && ends == n && max == capacity )); then
    echo "ok: capacity $capacity held under $n launches (max concurrent $max)"
    return 0
  fi
  echo "FAIL: capacity $capacity, $n launches -> started=$starts finished=$ends max=$max" >&2
  return 1
}

fail=0
run_case 5 2 || fail=1
run_case 4 1 || fail=1
run_case 5 3 || fail=1

if (( fail == 0 )); then
  echo "gate acceptance: all bounds held"
else
  echo "gate acceptance: FAILED" >&2
fi
exit "$fail"