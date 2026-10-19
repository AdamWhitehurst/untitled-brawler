#!/usr/bin/env bash
# Usage: bootstrap.sh
# Idempotent per-clone setup: the git review commit-msg hook, and git excludes for the files
# OpenRig writes into the repo and every worktree. Never edits tracked files.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

common="$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir)"
[ -f "$common/hooks/commit-msg" ] || git -C "$REPO" review install
exclude="$common/info/exclude"; mkdir -p "$(dirname "$exclude")"; touch "$exclude"
for p in CLAUDE.local.md /.claude/ /rig/AGENTS.md /rig/.agents/ /rig/.codex/; do
  grep -qxF "$p" "$exclude" || echo "$p" >> "$exclude"
done
echo "bootstrap ok: hook $common/hooks/commit-msg, excludes $exclude"
