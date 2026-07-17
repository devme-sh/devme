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
PIPE_SESSION="devme-setup-piped-secret-$$"
FIXTURE="$(mktemp -d /tmp/devme-setup-wizard.XXXXXX)"
HOME_DIR="$FIXTURE/home"
RUNTIME_DIR="$FIXTURE/runtime"
BIN_DIR="$FIXTURE/bin"
OPEN_LOG="$FIXTURE/opened-url.txt"

cleanup() {
  local status=$?
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  tmux kill-session -t "$PIPE_SESSION" 2>/dev/null || true
  (cd "$FIXTURE" && HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" "$DEVME" down >/dev/null 2>&1) || true
  rm -rf "$FIXTURE"
  exit "$status"
}
trap cleanup EXIT

mkdir -p "$HOME_DIR/.config/devme" "$RUNTIME_DIR" "$BIN_DIR"
git -C "$FIXTURE" init -q
printf '[hints]\nskills = "false"\n' > "$HOME_DIR/.config/devme/config.toml"
cat > "$BIN_DIR/browser-open" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$1" > "$DEVME_BROWSER_OPEN_LOG"
SH
chmod +x "$BIN_DIR/browser-open"
ln -s browser-open "$BIN_DIR/open"
ln -s browser-open "$BIN_DIR/xdg-open"
cat > "$FIXTURE/devme.toml" <<'TOML'
schema_version = 1

[stack]
env_file = ".env.auth.local"

[env.DISPLAY_NAME]
required = true
default = "Devme"
help = "Display name"

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

capture_pipe_until() {
  local pattern="$1"
  local output="$2"
  for _ in {1..100}; do
    tmux capture-pane -t "$PIPE_SESSION" -p > "$output"
    if grep -qF "$pattern" "$output"; then
      return 0
    fi
    sleep 0.1
  done
  echo "ASSERT FAIL: piped-secret command never showed '$pattern'" >&2
  cat "$output" >&2
  return 1
}

tmux new-session -d -s "$SESSION" -x 140 -y 28 \
  "cd '$FIXTURE' && PATH='$BIN_DIR:$PATH' DEVME_BROWSER_OPEN_LOG='$OPEN_LOG' HOME='$HOME_DIR' XDG_CONFIG_HOME='$HOME_DIR/.config' XDG_RUNTIME_DIR='$RUNTIME_DIR' '$DEVME'"

capture_until "DISPLAY_NAME" "$FIXTURE/initial.txt"
grep -qF "Agent help: ask your coding agent to finish this setup." "$FIXTURE/initial.txt"
grep -qF "It can read this wizard's live context with devme setup status." "$FIXTURE/initial.txt"
grep -qF "Enter Use default" "$FIXTURE/initial.txt"
grep -qF "›" "$FIXTURE/initial.txt"
if grep -qF "Type value" "$FIXTURE/initial.txt"; then
  echo "ASSERT FAIL: setup wizard showed redundant typing instructions" >&2
  cat "$FIXTURE/initial.txt" >&2
  exit 1
fi
tmux send-keys -t "$SESSION" Enter
capture_until "GOOGLE_WEB_CLIENT_ID" "$FIXTURE/url.txt"
grep -qF "DISPLAY_NAME=Devme" "$FIXTURE/.env.auth.local"
grep -qF "Tab Copy URL" "$FIXTURE/url.txt"
grep -qF "Shift+Tab Open browser" "$FIXTURE/url.txt"
grep -qF "›" "$FIXTURE/url.txt"
tmux send-keys -t "$SESSION" Tab
capture_until "Copied URL" "$FIXTURE/copied.txt"
tmux send-keys -t "$SESSION" BTab
capture_until "Opened https://console.example.test/credentials" "$FIXTURE/opened.txt"
for _ in {1..100}; do
  if grep -qF "https://console.example.test/credentials" "$OPEN_LOG" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
grep -qF "https://console.example.test/credentials" "$OPEN_LOG"

(cd "$FIXTURE" && HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$DEVME" setup set GOOGLE_WEB_CLIENT_ID \
  --value web.apps.googleusercontent.com --json >/dev/null)

capture_until "GOOGLE_CLIENT_SECRET" "$FIXTURE/secret.txt"
grep -qF "set by another process" "$FIXTURE/secret.txt" || {
  echo "ASSERT FAIL: human wizard did not acknowledge the agent update" >&2
  cat "$FIXTURE/secret.txt" >&2
  exit 1
}

tmux send-keys -t "$SESSION" "oauth-client-secret"
capture_until "••••" "$FIXTURE/masked.txt"
if grep -qF "oauth-client-secret" "$FIXTURE/masked.txt"; then
  echo "ASSERT FAIL: secret input was rendered in plaintext" >&2
  cat "$FIXTURE/masked.txt" >&2
  exit 1
fi
tmux send-keys -t "$SESSION" Enter

capture_until "actions:" "$FIXTURE/complete.txt"
grep -qF "GOOGLE_CLIENT_SECRET=oauth-client-secret" "$FIXTURE/.env.auth.local"
(cd "$FIXTURE" && HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" "$DEVME" setup status --json) > "$FIXTURE/status.json"
grep -qF '"status": "complete"' "$FIXTURE/status.json"

PIPE_FIXTURE="$FIXTURE/piped-secret"
mkdir -p "$PIPE_FIXTURE"
cat > "$PIPE_FIXTURE/devme.toml" <<'TOML'
schema_version = 1

[env.API_SECRET]
required = true
secret = true
TOML
printf 'piped-secret-value\n' > "$PIPE_FIXTURE/value"
tmux new-session -d -s "$PIPE_SESSION" -x 100 -y 20 \
  "cd '$PIPE_FIXTURE' && export HOME='$HOME_DIR' XDG_CONFIG_HOME='$HOME_DIR/.config' XDG_RUNTIME_DIR='$RUNTIME_DIR'; cat value | '$DEVME' setup set API_SECRET; printf '\n__SETUP_DONE__\n'; sleep 30"
capture_pipe_until "__SETUP_DONE__" "$PIPE_FIXTURE/output.txt"
grep -qF "status: complete" "$PIPE_FIXTURE/output.txt"
if grep -qF "Environment setup:" "$PIPE_FIXTURE/output.txt"; then
  echo "ASSERT FAIL: piped secret with terminal stdout used human output" >&2
  cat "$PIPE_FIXTURE/output.txt" >&2
  exit 1
fi
if grep -qF "piped-secret-value" "$PIPE_FIXTURE/output.txt"; then
  echo "ASSERT FAIL: piped secret appeared in terminal output" >&2
  cat "$PIPE_FIXTURE/output.txt" >&2
  exit 1
fi
tmux kill-session -t "$PIPE_SESSION"

tmux send-keys -t "$SESSION" q
echo "ok [controls] setup URL can be opened or copied"
echo "ok [live] agent-supplied values advanced the human wizard without recheck"
echo "ok [typing] values accept direct input without an activation key"
echo "ok [secret] human secret input stayed masked"
echo "ok [status] configured env file is the live source of truth"
echo "ok [piped secret] non-interactive stdin selects redacted TOON on terminal stdout"
