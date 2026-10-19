#!/usr/bin/env bash
# Usage: remove-builder.sh <mission>-<slice>
# Removes the builder seat, unregisters the slice from the PR watcher, and removes the worktree.
# Deletes the branch only when git sees it merged.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

name="${1:?usage: remove-builder.sh <mission>-<slice>}"
[[ "$name" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "bad name: $name" >&2; exit 2; }
wt_dir="$WT_ROOT/$name"

rig remove "$(rig_id)" "build.$name" --json
rm -f "$WATCH_DIR/slices/$name.json"
[ ! -d "$wt_dir" ] || git -C "$REPO" worktree remove "$wt_dir"
git -C "$REPO" branch -d "$name" 2>/dev/null && echo "deleted merged branch $name" || echo "kept branch $name"
