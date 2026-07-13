#!/usr/bin/env bash
# Drive bare Devme from a native workspace member and prove its focused
# session stays live for the TUI lifetime.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEVME="${1:-$REPO_ROOT/target/release/devme}"
FIXTURE="$REPO_ROOT/crates/cli/tests/fixtures/native-mobile-workspace"
SESSION="devme-native-session-$$"
ROOT="$(mktemp -d /tmp/devme-native-session.XXXXXX)"
RUNTIME="$ROOT/runtime"
HOME_DIR="$ROOT/home"

if [[ ! -x "$DEVME" ]]; then
  echo "devme binary not found at $DEVME" >&2
  exit 2
fi
command -v tmux >/dev/null || { echo "tmux required" >&2; exit 2; }

cp -R "$FIXTURE/." "$ROOT/project"
mkdir -p "$RUNTIME" "$HOME_DIR/.config/devme"
printf '[hints]\nskills = "false"\n' > "$HOME_DIR/.config/devme/config.toml"
git -C "$ROOT/project" init -q

run_devme() {
  (cd "$ROOT/project/apps/ios" && \
    HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
    XDG_RUNTIME_DIR="$RUNTIME" "$DEVME" "$@")
}

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  run_devme down >/dev/null 2>&1 || true
  rm -rf "$ROOT"
}
trap cleanup EXIT

tmux new-session -d -s "$SESSION" -x 120 -y 32 \
  "cd '$ROOT/project/apps/ios' && HOME='$HOME_DIR' XDG_CONFIG_HOME='$HOME_DIR/.config' XDG_RUNTIME_DIR='$RUNTIME' '$DEVME'"

deadline=$((SECONDS + 15))
until [[ -f "$ROOT/project/apps/ios/.launched" && -f "$ROOT/project/apps/ios/.logs-started" ]]; do
  if (( SECONDS >= deadline )); then
    echo "focused session did not launch before deadline" >&2
    tmux capture-pane -t "$SESSION" -p >&2 || true
    exit 1
  fi
  sleep 1
done

sessions="$(run_devme sessions --output json)"
printf '%s' "$sessions" | grep -Eq '"name"[[:space:]]*:[[:space:]]*"ios::dev"'
printf '%s' "$sessions" | grep -Eq '"status"[[:space:]]*:[[:space:]]*"ready"'

pane="$(tmux capture-pane -t "$SESSION" -p)"
if ! printf '%s' "$pane" | grep -q 'device-logs'; then
  echo "focused TUI did not render the session log service" >&2
  printf '%s\n' "$pane" >&2
  exit 1
fi
tmux send-keys -t "$SESSION" q
sleep 0.2
tmux send-keys -t "$SESSION" q

deadline=$((SECONDS + 15))
while tmux has-session -t "$SESSION" 2>/dev/null; do
  if (( SECONDS >= deadline )); then
    echo "TUI did not exit after q" >&2
    tmux capture-pane -t "$SESSION" -p >&2 || true
    exit 1
  fi
  sleep 1
done

echo "native workspace focused session smoke passed"
