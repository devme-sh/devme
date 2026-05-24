#!/usr/bin/env bash
#
# tui-smoke.sh — drive the TUI in a headless tmux session and assert the
# visible grid matches expectations. Re-run after any render or event-loop
# change as a sanity check.
#
# Usage: scripts/tui-smoke.sh [path/to/release/devme]
#
# Assumes tmux is installed. Captures are stored in /tmp/devme-tui-cap*.txt
# for easy diffing.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEVME="${1:-$REPO_ROOT/target/release/devme}"
SMOKE_DIR="$REPO_ROOT/examples/smoke"
SESSION="devme-smoke-$$"

if [[ ! -x "$DEVME" ]]; then
  echo "devme binary not found at $DEVME — run 'cargo build --release -p devme -p devme-supervisor -p devme-tui' first" >&2
  exit 2
fi
if [[ ! -d "$SMOKE_DIR" ]]; then
  echo "smoke env missing at $SMOKE_DIR" >&2
  exit 2
fi
command -v tmux >/dev/null || { echo "tmux required" >&2; exit 2; }

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  (cd "$SMOKE_DIR" && "$DEVME" down >/dev/null 2>&1) || true
}
trap cleanup EXIT

# Make sure no stale daemon is around.
(cd "$SMOKE_DIR" && "$DEVME" down >/dev/null 2>&1) || true

tmux new-session -d -s "$SESSION" -x 120 -y 30 "cd $SMOKE_DIR && $DEVME"
sleep 3

# Helper: capture, grep, fail with full pane on miss.
assert_contains() {
  local needle="$1" label="$2"
  local cap="/tmp/devme-tui-cap-$label.txt"
  tmux capture-pane -t "$SESSION" -p > "$cap"
  if ! grep -qF -- "$needle" "$cap"; then
    echo "ASSERT FAIL [$label]: missing '$needle'" >&2
    echo "--- captured pane ---" >&2
    cat "$cap" >&2
    exit 1
  fi
  echo "  ok  [$label] saw '$needle'"
}

echo "1. initial render"
assert_contains "stacks"  "stacks-pane"
assert_contains "tools"   "tools-pane"
assert_contains "tick"    "tabs"
assert_contains "flaky"   "tabs"
assert_contains "boom"    "tabs"

echo "2. page-up triggers PAUSED indicator"
tmux send-keys -t "$SESSION" "b"
sleep 1
assert_contains "PAUSED"        "paused-pill"
assert_contains "G to follow"   "paused-hint"

echo "3. viewport stable while paused (capture twice, log content matches)"
# We compare only the log text region — the PAUSED counter advances and
# the scrollbar thumb on the right edge moves as the buffer grows, both
# expected. The actual log lines must stay still.
extract_logs() {
  # Strip the PAUSED line (counter advances), then chop off everything
  # right of column 110 (scrollbar gutter + border). Pane is 120 wide.
  grep -v PAUSED "$1" | cut -c1-110
}
tmux capture-pane -t "$SESSION" -p > /tmp/devme-tui-cap-stable-a.txt
sleep 3
tmux capture-pane -t "$SESSION" -p > /tmp/devme-tui-cap-stable-b.txt
if ! diff <(extract_logs /tmp/devme-tui-cap-stable-a.txt) \
          <(extract_logs /tmp/devme-tui-cap-stable-b.txt) >/dev/null; then
  echo "ASSERT FAIL: viewport drifted while paused" >&2
  diff <(extract_logs /tmp/devme-tui-cap-stable-a.txt) \
       <(extract_logs /tmp/devme-tui-cap-stable-b.txt) >&2
  exit 1
fi
echo "  ok  [stable] log content unchanged across 3s while paused"

echo "4. G returns to live tail"
tmux send-keys -t "$SESSION" "G"
sleep 1
tmux capture-pane -t "$SESSION" -p > /tmp/devme-tui-cap-tail.txt
if grep -qF "PAUSED" /tmp/devme-tui-cap-tail.txt; then
  echo "ASSERT FAIL: PAUSED still visible after G" >&2
  exit 1
fi
echo "  ok  [tail] PAUSED cleared after G"

echo "5. tab switch with l shows another service's logs"
tmux send-keys -t "$SESSION" "l"
sleep 1
assert_contains "flaky" "tab-switch-flaky"

echo "6. q quits and shuts down the daemon"
tmux send-keys -t "$SESSION" "q"
sleep 2
# Daemon should be gone — `down` should say "no daemon running".
if ! (cd "$SMOKE_DIR" && "$DEVME" down 2>&1 | grep -qF "no daemon running"); then
  echo "ASSERT FAIL: daemon still alive after q" >&2
  exit 1
fi
echo "  ok  [shutdown] q tore the daemon down"

echo
echo "all good"
