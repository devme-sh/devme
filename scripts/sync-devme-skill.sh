#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_file="$repo_root/crates/config/skill/SKILL.md"
repository_file="$repo_root/.agents/skills/devme/SKILL.md"

case "${1:-sync}" in
  sync)
    mkdir -p "$(dirname "$repository_file")"
    cp "$source_file" "$repository_file"
    ;;
  --check)
    if ! cmp -s "$source_file" "$repository_file"; then
      echo "error: repository Devme skill is stale" >&2
      echo "help: run scripts/sync-devme-skill.sh" >&2
      exit 1
    fi
    ;;
  *)
    echo "usage: scripts/sync-devme-skill.sh [--check]" >&2
    exit 2
    ;;
esac
