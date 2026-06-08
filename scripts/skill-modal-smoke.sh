#!/usr/bin/env bash
# Headless verification of the TUI skill modals (install + update + auto-update),
# driven through tmux. Isolated HOME so it never touches the real ~/.claude or
# ~/.config. Patterned after scripts/tui-smoke.sh.
set -uo pipefail

DEVME="$HOME/.cargo/bin/devme"
TMUX_BIN="$(command -v tmux)"
ROOT="$(mktemp -d /tmp/skill-modal.XXXXXX)"
SESSION="skill-modal-$$"
fails=0

cleanup() {
  "$TMUX_BIN" kill-session -t "$SESSION" 2>/dev/null || true
}
trap cleanup EXIT

# Each test runs the TUI under a fresh fake HOME + project dir.
new_env() {
  local name="$1"
  FAKE_HOME="$ROOT/$name/home"; PROJ="$ROOT/$name/proj"
  mkdir -p "$FAKE_HOME" "$PROJ"
  printf 'schema_version = 1\n[service.idle]\ncmd = "sleep 600"\n' > "$PROJ/devme.toml"
}

launch() {
  # Fresh fake HOME, unset XDG_CONFIG_HOME so config lands in $FAKE_HOME/.config.
  "$TMUX_BIN" new-session -d -s "$SESSION" -x 120 -y 32 \
    "cd $PROJ && env -u XDG_CONFIG_HOME HOME=$FAKE_HOME $DEVME"
  sleep 3
}

cap() { "$TMUX_BIN" capture-pane -t "$SESSION" -p; }

assert_has() {
  local needle="$1" label="$2"
  if cap | grep -qF -- "$needle"; then
    echo "  ok  [$label] saw '$needle'"
  else
    echo "  FAIL [$label] missing '$needle'"; echo "--- pane ---"; cap; echo "------------"
    fails=$((fails + 1))
  fi
}
assert_absent() {
  local needle="$1" label="$2"
  if cap | grep -qF -- "$needle"; then
    echo "  FAIL [$label] still saw '$needle'"; fails=$((fails + 1))
  else
    echo "  ok  [$label] '$needle' gone"
  fi
}
quit_tui() {
  "$TMUX_BIN" send-keys -t "$SESSION" "q"; sleep 2
  (cd "$PROJ" && env -u XDG_CONFIG_HOME HOME="$FAKE_HOME" "$DEVME" down >/dev/null 2>&1) || true
  "$TMUX_BIN" kill-session -t "$SESSION" 2>/dev/null || true
}

echo "TEST 1 — install modal appears when nothing is installed"
new_env t1
launch
assert_has "AI skill"          "install-title"
assert_has "Install it"        "install-prompt"
assert_has "install globally"  "install-global-opt"
echo "  → press 'i' (install into project)"
"$TMUX_BIN" send-keys -t "$SESSION" "i"; sleep 2
assert_absent "Install it"     "modal-dismissed-after-i"
quit_tui
if [ -f "$PROJ/.claude/skills/devme/SKILL.md" ]; then
  echo "  ok  [install-wrote-file] .claude/skills/devme/SKILL.md exists"
else
  echo "  FAIL [install-wrote-file] file not written"; fails=$((fails + 1))
fi

echo "TEST 2 — 'n' dismisses the install modal without installing"
new_env t2
launch
assert_has "AI skill" "install-title-2"
"$TMUX_BIN" send-keys -t "$SESSION" "n"; sleep 1
assert_absent "Install it" "dismissed-by-n"
assert_has "help" "footer-back"        # footer keybindings visible again
quit_tui
if [ -f "$PROJ/.claude/skills/devme/SKILL.md" ]; then
  echo "  FAIL [n-no-install] file was written on dismiss"; fails=$((fails + 1))
else
  echo "  ok  [n-no-install] nothing installed after 'n'"
fi

echo "TEST 3 — update modal appears for a stale devme-managed install"
new_env t3
# Install current, then forge a stale record (old body on disk + matching hash).
(cd "$PROJ" && env -u XDG_CONFIG_HOME HOME="$FAKE_HOME" "$DEVME" skill install >/dev/null)
F="$PROJ/.claude/skills/devme/SKILL.md"; OLD="--- old skill v0.1.2 ---"
printf '%s' "$OLD" > "$F"
H=$(python3 -c "
s=b'''$OLD'''
h=0xcbf29ce484222325
for b in s: h^=b; h=(h*0x100000001b3)&0xFFFFFFFFFFFFFFFF
print(f'{h:016x}')")
python3 - "$FAKE_HOME/.config/devme/config.toml" "$H" <<'PY'
import re,sys
p,h=sys.argv[1],sys.argv[2]
t=open(p).read()
t=re.sub(r'version = "[^"]*"\nhash = "[^"]*"', f'version = "0.1.2"\nhash = "{h}"', t)
open(p,"w").write(t)
PY
launch
assert_has "out of date" "update-prompt"
assert_has "update now"  "update-opt"
echo "  → press 'u' (update now)"
"$TMUX_BIN" send-keys -t "$SESSION" "u"; sleep 2
quit_tui
if head -1 "$F" | grep -qF -- "---" && grep -qF "name: devme" "$F"; then
  echo "  ok  [update-refreshed] on-disk skill is back to embedded content"
else
  echo "  FAIL [update-refreshed] file not refreshed (first line: $(head -1 "$F"))"; fails=$((fails + 1))
fi

echo "TEST 4 — auto_update refreshes silently, no modal"
new_env t4
(cd "$PROJ" && env -u XDG_CONFIG_HOME HOME="$FAKE_HOME" "$DEVME" skill install >/dev/null)
(cd "$PROJ" && env -u XDG_CONFIG_HOME HOME="$FAKE_HOME" "$DEVME" config set skill.auto_update true >/dev/null)
F="$PROJ/.claude/skills/devme/SKILL.md"; OLD="--- old skill v0.1.2 ---"
printf '%s' "$OLD" > "$F"
H=$(python3 -c "
s=b'''$OLD'''
h=0xcbf29ce484222325
for b in s: h^=b; h=(h*0x100000001b3)&0xFFFFFFFFFFFFFFFF
print(f'{h:016x}')")
python3 - "$FAKE_HOME/.config/devme/config.toml" "$H" <<'PY'
import re,sys
p,h=sys.argv[1],sys.argv[2]
t=open(p).read()
t=re.sub(r'version = "[^"]*"\nhash = "[^"]*"', f'version = "0.1.2"\nhash = "{h}"', t)
open(p,"w").write(t)
PY
launch
assert_absent "out of date" "no-modal-when-auto"
assert_absent "AI skill"    "no-install-modal-when-auto"
quit_tui
if grep -qF "name: devme" "$F"; then
  echo "  ok  [auto-refreshed] file silently refreshed under auto_update"
else
  echo "  FAIL [auto-refreshed] file not refreshed"; fails=$((fails + 1))
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "ALL PASS"; rm -rf "$ROOT"
else
  echo "$fails assertion(s) FAILED — artifacts kept at $ROOT"
fi
exit "$fails"
