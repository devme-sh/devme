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
SMOKE_SRC="$REPO_ROOT/examples/smoke"
SESSION="devme-smoke-$$"

if [[ ! -x "$DEVME" ]]; then
  echo "devme binary not found at $DEVME — run 'cargo build --release -p devme -p devme-supervisor -p devme-tui' first" >&2
  exit 2
fi
if [[ ! -d "$SMOKE_SRC" ]]; then
  echo "smoke env missing at $SMOKE_SRC" >&2
  exit 2
fi
command -v tmux >/dev/null || { echo "tmux required" >&2; exit 2; }

# The TUI is worktree-aware: it resolves the *enclosing git repo* and shows
# that repo's worktree stacks. Run the fixture from inside this repo and it
# would render the devme repo's own stack (`build`), not tick/flaky/boom —
# so copy it to an isolated throwaway git repo first.
SMOKE_DIR="$(mktemp -d /tmp/devme-tui-smoke.XXXXXX)"
cp "$SMOKE_SRC/devme.toml" "$SMOKE_DIR/"
git -C "$SMOKE_DIR" init -q

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  (cd "$SMOKE_DIR" && "$DEVME" down >/dev/null 2>&1) || true
  rm -rf "$SMOKE_DIR"
}
trap cleanup EXIT

tmux new-session -d -s "$SESSION" -x 120 -y 30 "cd $SMOKE_DIR && $DEVME"
# Long enough for the daemon to spawn and for tick (10 lines/s) to overflow
# the ~24-row viewport, so page-up in step 2 has somewhere to scroll to.
sleep 5

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
  # Strip the PAUSED line (counter advances), then keep only the left side
  # of the log pane: notification toasts (flaky crashing/recovering) overlay
  # columns ~53+ and come and go between captures, and the scrollbar thumb
  # moves as the buffer grows — neither is viewport drift. The log text
  # itself lives in the first ~50 columns and must stay still.
  grep -v PAUSED "$1" | cut -c1-52
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
# Quit tears down every worktree's daemon plus the shared supervisor; give it
# up to 15s rather than a fixed beat. Done when `down` finds nothing to stop.
deadline=$((SECONDS + 15))
until (cd "$SMOKE_DIR" && "$DEVME" down 2>&1 | grep -qF "no daemon running"); do
  if (( SECONDS >= deadline )); then
    echo "ASSERT FAIL: daemon still alive 15s after q" >&2
    exit 1
  fi
  sleep 1
done
echo "  ok  [shutdown] q tore the daemon down"

echo
echo "all good"
