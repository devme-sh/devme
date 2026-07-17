#!/usr/bin/env bash
# Verify that the interactive environment wizard exposes URL controls and
# advances live when another process supplies values through the agent CLI.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEVME="${1:-$REPO_ROOT/target/debug/devme}"
if [[ "$DEVME" != /* ]]; then
  DEVME="$(cd "$(dirname "$DEVME")" && pwd)/$(basename "$DEVME")"
fi
SESSION="devme-setup-wizard-$$"
FIXTURE="$(mktemp -d /tmp/devme-setup-wizard.XXXXXX)"
HOME_DIR="$FIXTURE/home"
RUNTIME_DIR="$FIXTURE/runtime"

cleanup() {
  local status=$?
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  (cd "$FIXTURE" && HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" "$DEVME" down >/dev/null 2>&1) || true
  rm -rf "$FIXTURE"
  exit "$status"
}
trap cleanup EXIT

mkdir -p "$HOME_DIR/.config/devme" "$RUNTIME_DIR"
git -C "$FIXTURE" init -q
printf '[hints]\nskills = "false"\n' > "$HOME_DIR/.config/devme/config.toml"
cat > "$FIXTURE/devme.toml" <<'TOML'
schema_version = 1

[stack]
env_file = ".env.auth.local"

[env.GOOGLE_WEB_CLIENT_ID]
required = true
setup_url = "https://console.example.test/credentials"
help = "Google web OAuth client ID"

[env.GOOGLE_CLIENT_SECRET]
required = true
secret = true
setup_url = "https://console.example.test/credentials"
help = "Google web OAuth client secret"

[task.verify]
kind = "check"
cmd = "true"
TOML

capture_until() {
  local pattern="$1"
  local output="$2"
  for _ in {1..100}; do
    tmux capture-pane -t "$SESSION" -p > "$output"
    if grep -qF "$pattern" "$output"; then
      return 0
    fi
    sleep 0.1
  done
  echo "ASSERT FAIL: setup wizard never showed '$pattern'" >&2
  cat "$output" >&2
  return 1
}

tmux new-session -d -s "$SESSION" -x 140 -y 28 \
  "cd '$FIXTURE' && HOME='$HOME_DIR' XDG_CONFIG_HOME='$HOME_DIR/.config' XDG_RUNTIME_DIR='$RUNTIME_DIR' '$DEVME'"

capture_until "GOOGLE_WEB_CLIENT_ID" "$FIXTURE/initial.txt"
grep -qF "Open browser" "$FIXTURE/initial.txt"
grep -qF "Copy URL" "$FIXTURE/initial.txt"

(cd "$FIXTURE" && HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$DEVME" setup set GOOGLE_WEB_CLIENT_ID \
  --value web.apps.googleusercontent.com --json >/dev/null)

capture_until "GOOGLE_CLIENT_SECRET" "$FIXTURE/secret.txt"
grep -qF "set by another process" "$FIXTURE/secret.txt" || {
  echo "ASSERT FAIL: human wizard did not acknowledge the agent update" >&2
  cat "$FIXTURE/secret.txt" >&2
  exit 1
}

tmux send-keys -t "$SESSION" Enter
tmux send-keys -t "$SESSION" "local-secret"
capture_until "••••" "$FIXTURE/masked.txt"
if grep -qF "local-secret" "$FIXTURE/masked.txt"; then
  echo "ASSERT FAIL: secret input was rendered in plaintext" >&2
  cat "$FIXTURE/masked.txt" >&2
  exit 1
fi
tmux send-keys -t "$SESSION" Enter

capture_until "actions:" "$FIXTURE/complete.txt"
(cd "$FIXTURE" && HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$DEVME" setup status --json) > "$FIXTURE/status.json"
grep -qF '"status": "complete"' "$FIXTURE/status.json"

tmux send-keys -t "$SESSION" q
echo "ok [controls] setup URL can be opened or copied"
echo "ok [live] agent-supplied values advanced the human wizard without recheck"
echo "ok [secret] human secret input stayed masked"
echo "ok [status] configured env file is the live source of truth"
