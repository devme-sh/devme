#!/usr/bin/env bash
# Drive an agent-owned CLI task and service command while a separate TUI stays live.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEVME="${1:-$REPO_ROOT/target/release/devme}"
SESSION="devme-agent-tui-$$"
FIXTURE="$(mktemp -d /tmp/devme-agent-tui-smoke.XXXXXX)"
HOME_DIR="$FIXTURE/home"
RUNTIME_DIR="$FIXTURE/runtime"
TASK_OUTPUT="$FIXTURE/task-output.txt"
AGENT_PID=""

if [[ ! -x "$DEVME" ]]; then
  echo "devme binary not found at $DEVME" >&2
  exit 2
fi
command -v tmux >/dev/null || { echo "tmux required" >&2; exit 2; }

mkdir -p "$HOME_DIR/.config/devme" "$RUNTIME_DIR"
git -C "$FIXTURE" init -q
cat > "$FIXTURE/devme.toml" <<'TOML'
schema_version = 1

[task.agent-check]
kind = "check"
description = "Agent-driven check"
cmd = "echo agent-task-started; sleep 5; echo agent-task-finished"

[service.api]
cmd = "while true; do echo api-heartbeat; sleep 0.2; done"
TOML
cat > "$HOME_DIR/.config/devme/config.toml" <<'TOML'
[hints]
skills = "false"
TOML

devme_env=(
  HOME="$HOME_DIR"
  XDG_CONFIG_HOME="$HOME_DIR/.config"
  XDG_RUNTIME_DIR="$RUNTIME_DIR"
)

cleanup() {
  if [[ -n "$AGENT_PID" ]]; then
    kill "$AGENT_PID" 2>/dev/null || true
    wait "$AGENT_PID" 2>/dev/null || true
  fi
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  (cd "$FIXTURE" && env "${devme_env[@]}" "$DEVME" down >/dev/null 2>&1) || true
  rm -rf "$FIXTURE"
}
trap cleanup EXIT

capture_until() {
  local file="$1"
  local pattern="$2"
  for _ in {1..100}; do
    tmux capture-pane -t "$SESSION" -p > "$file"
    if grep -qF "$pattern" "$file"; then
      return 0
    fi
    sleep 0.1
  done
  echo "ASSERT FAIL: TUI never showed '$pattern'" >&2
  cat "$file" >&2
  return 1
}

tmux new-session -d -s "$SESSION" -x 120 -y 30 \
  "cd '$FIXTURE' && env HOME='$HOME_DIR' XDG_CONFIG_HOME='$HOME_DIR/.config' XDG_RUNTIME_DIR='$RUNTIME_DIR' '$DEVME'"
capture_until "$FIXTURE/initial.txt" "actions:"

(
  cd "$FIXTURE"
  env "${devme_env[@]}" "$DEVME" run agent-check --output toon > "$TASK_OUTPUT" 2>&1
) &
AGENT_PID=$!

capture_until "$FIXTURE/running.txt" "/ agent-check  agent-task-started"
grep -qF "◌ agent check" "$FIXTURE/running.txt" || {
  echo "ASSERT FAIL: Actions did not mark the agent-owned task as running" >&2
  cat "$FIXTURE/running.txt" >&2
  exit 1
}

tmux send-keys -t "$SESSION" a
capture_until "$FIXTURE/stacks.txt" "stacks"
tmux send-keys -t "$SESSION" a
capture_until "$FIXTURE/actions-again.txt" "actions:"

(cd "$FIXTURE" && env "${devme_env[@]}" "$DEVME" start api)
capture_until "$FIXTURE/service-live.txt" "1/1 running"
grep -qF "api-heartbeat" "$FIXTURE/service-live.txt" || {
  echo "ASSERT FAIL: TUI did not stream logs from the agent-started service" >&2
  cat "$FIXTURE/service-live.txt" >&2
  exit 1
}

if ! wait "$AGENT_PID"; then
  AGENT_PID=""
  echo "ASSERT FAIL: agent-owned task failed" >&2
  cat "$TASK_OUTPUT" >&2
  exit 1
fi
AGENT_PID=""
capture_until "$FIXTURE/finished.txt" "/ agent-check  succeeded"

tmux send-keys -t "$SESSION" n
capture_until "$FIXTURE/notifications.txt" "agent-check succeeded in"
grep -qF "agent-check running in" "$FIXTURE/notifications.txt" || {
  echo "ASSERT FAIL: task-start notification was not retained" >&2
  cat "$FIXTURE/notifications.txt" >&2
  exit 1
}
grep -qF "api ready" "$FIXTURE/notifications.txt" || {
  echo "ASSERT FAIL: service-ready notification was not retained" >&2
  cat "$FIXTURE/notifications.txt" >&2
  exit 1
}

echo "ok [task-live] agent-owned task and progress appeared in the TUI"
echo "ok [interactive] Actions and Stacks stayed keyboard-usable during the task"
echo "ok [service-live] agent-started service state and logs streamed live"
echo "ok [notifications] task start, task finish, and service ready were retained"
