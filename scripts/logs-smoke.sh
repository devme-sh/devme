#!/usr/bin/env bash
#
# logs-smoke.sh — end-to-end smoke for the agent-first log surface:
# `devme logs` (--tail exactness, --since, --json/NDJSON, stream tags,
# interleave, step redirect) and `devme doctor` (error digest, per-node
# zoom). Plain shell — no tmux needed; these are one-shot CLI commands.
#
# Usage: scripts/logs-smoke.sh [path/to/release/devme]
#
# Runs in an isolated fixture under /tmp so it never touches a real
# project's daemon or log history.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEVME="${1:-$REPO_ROOT/target/release/devme}"
FIXTURE="$(mktemp -d /tmp/devme-logs-smoke.XXXXXX)"

if [[ ! -x "$DEVME" ]]; then
  echo "devme binary not found at $DEVME — run 'cargo build --release' first" >&2
  exit 2
fi
command -v jq >/dev/null || { echo "jq required" >&2; exit 2; }

cleanup() {
  (cd "$FIXTURE" && "$DEVME" down >/dev/null 2>&1) || true
  rm -rf "$FIXTURE"
}
trap cleanup EXIT

PASS=0
FAIL=0
check() { # check <label> <command...>
  local label="$1"; shift
  if "$@" >/dev/null 2>&1; then
    echo "  ok: $label"; PASS=$((PASS + 1))
  else
    echo "FAIL: $label" >&2; FAIL=$((FAIL + 1))
  fi
}

cat > "$FIXTURE/devme.toml" <<'EOF'
schema_version = 1

[step.warmup]
check = "true"

[step.toolcheck]
check = "sh -c 'echo checking tools; echo missing frobnicator >&2; exit 1'"

[service.web]
cmd = "sh -c 'while true; do echo out-line; echo err-line >&2; sleep 0.3; done'"

[service.crashy]
cmd = "sh -c 'echo starting up; echo fatal: cannot bind >&2; exit 1'"
EOF

cd "$FIXTURE"
"$DEVME" up --yes >/dev/null 2>&1 &
sleep 8

echo "— logs —"

# --tail N returns exactly N lines, deterministically (no live-line race).
for i in 1 2 3; do
  check "tail 6 is exactly 6 lines (run $i)" \
    test "$("$DEVME" logs --tail 6 2>/dev/null | wc -l | tr -d ' ')" = 6
done

# No spurious "history rotated away" warning when only tail-clipping.
check "no rotation warning on plain --tail" \
  test -z "$("$DEVME" logs --tail 6 2>&1 >/dev/null)"

# No-arg interleaves multiple services into one stream.
check "interleave shows both services" \
  bash -c "$DEVME logs --tail 40 2>/dev/null | grep -q 'web |' && $DEVME logs --tail 40 2>/dev/null | grep -q 'crashy |'"

# --json is NDJSON with ts/service/stream/text; stderr lines are tagged.
check "json record has all fields" \
  bash -c "$DEVME logs web --tail 5 --json 2>/dev/null | head -1 | jq -e 'has(\"ts\") and has(\"service\") and has(\"stream\") and has(\"text\")'"
check "stderr lines are stream-tagged" \
  bash -c "$DEVME logs web --tail 10 --json 2>/dev/null | jq -es '[.[] | select(.stream == \"stderr\")] | length > 0'"

# --since anchors the window in time.
check "--since 2s returns recent lines" \
  bash -c "test \"\$($DEVME logs web --since 2s 2>/dev/null | wc -l)\" -gt 0"
check "--since far future returns nothing" \
  bash -c "test \"\$($DEVME logs web --since 99999999999999 2>/dev/null | wc -l | tr -d ' ')\" = 0"

# Channel partition: steps are refused with a doctor pointer; the
# provisioning tree never renders; unknown names error immediately.
check "logs <step> redirects to doctor" \
  bash -c "$DEVME logs warmup 2>&1 | grep -q 'devme doctor warmup'"
check "logs never renders the dependency-check tree" \
  bash -c "! $DEVME logs warmup 2>&1 | grep -q 'Check dependencies'"
check "logs unknown name errors" \
  bash -c "$DEVME logs nosuch 2>&1 | grep -q 'no service or step named'"

echo "— doctor —"

DIGEST="$("$DEVME" doctor --tail 5 2>/dev/null)"
check "digest is unhealthy (crashy + toolcheck failed)" \
  bash -c "echo '$DIGEST' | jq -e '.status == \"unhealthy\"'"
check "digest recent_errors is stderr-only" \
  bash -c "echo '$DIGEST' | jq -e '.services[] | select(.name == \"web\") | .recent_errors | all(startswith(\"[stderr]\"))'"
check "failed step output is inline" \
  bash -c "echo '$DIGEST' | jq -e '.steps[] | select(.name == \"toolcheck\") | .output | length > 0'"
check "passed step has no output blob" \
  bash -c "echo '$DIGEST' | jq -e '.steps[] | select(.name == \"warmup\") | has(\"output\") | not'"

check "doctor <step> zooms into check output" \
  bash -c "$DEVME doctor toolcheck 2>/dev/null | jq -e '.kind == \"step\" and (.output | length > 0)'"
check "doctor <service> has recent_errors + recent_logs" \
  bash -c "$DEVME doctor crashy 2>/dev/null | jq -e '.kind == \"service\" and has(\"recent_errors\") and has(\"recent_logs\")'"
check "doctor unknown name errors" \
  bash -c "$DEVME doctor nosuch 2>&1 | grep -q 'no service or step named'"

echo
echo "passed $PASS, failed $FAIL"
[[ "$FAIL" = 0 ]]
