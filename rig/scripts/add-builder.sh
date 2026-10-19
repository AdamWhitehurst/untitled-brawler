#!/usr/bin/env bash
# Usage: add-builder.sh <project> <mission> <slice> [--base <ref>] [--dry-run]
# Creates the worktree and branch <mission>-<slice> under ~/wt/<repo>/, grows build.<mission>-<slice>,
# and registers the slice with the PR watcher. Reuses an existing worktree.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

base=""; dry_run=0; pos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --base) base="${2:?--base needs a ref}"; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -*) echo "unknown arg: $1" >&2; exit 2 ;;
    *) pos+=("$1"); shift ;;
  esac
done
[ "${#pos[@]}" -eq 3 ] || { echo "usage: add-builder.sh <project> <mission> <slice> [--base <ref>] [--dry-run]" >&2; exit 2; }
project="${pos[0]}"; mission="${pos[1]}"; slice="${pos[2]}"
for v in "$project" "$mission" "$slice"; do
  [[ "$v" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "ids must be lowercase kebab-case: $v" >&2; exit 2; }
done

slice_dir="$WS_ROOT/projects/$project/missions/$mission/slices/$slice"
[ -f "$slice_dir/SPEC.md" ] || { echo "no slice SPEC at $slice_dir/SPEC.md" >&2; exit 1; }
name="$mission-$slice"; wt_dir="$WT_ROOT/$name"; session="build-$name@$RIG_NAME"
if [ -z "$base" ]; then
  base="$(git -C "$REPO" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || echo master)"
fi

if [ "$dry_run" = 1 ]; then
  printf 'dry run: nothing created\nworktree: %s%s\nbranch:   %s\nbase:     %s\nsession:  %s\nslice:    %s\n' \
    "$wt_dir" "$([ -d "$wt_dir" ] && echo ' (exists, would reuse)')" "$name" "$base" "$session" "$slice_dir"
  exit 0
fi

"$RIG_ROOT/scripts/bootstrap.sh" >/dev/null
git -C "$REPO" fetch origin "${base#origin/}" >/dev/null 2>&1 || echo "note: fetch failed; using local $base" >&2
if [ -d "$wt_dir" ]; then
  echo "worktree exists, reusing: $wt_dir"
else
  mkdir -p "$WT_ROOT"
  git -C "$REPO" worktree add --no-track -b "$name" "$wt_dir" "$base"
fi

frag="$(mktemp "${TMPDIR:-/tmp}/builder-member.XXXXXX.yaml")"; trap 'rm -f "$frag"' EXIT
sed -e "s|@@ID@@|$name|g" -e "s|@@CWD@@|$wt_dir|g" -e "s|@@SLICE@@|$slice_dir|g" -e "s|@@BASE@@|$base|g" \
  "$RIG_ROOT/members/builder.member.yaml.tmpl" > "$frag"
rig add "$RIG_NAME" build "$frag" --rig-root "$RIG_ROOT" --json

mkdir -p "$WATCH_DIR/slices"
python3 -Ic 'import json,sys; k=["session","worktree","base","project","mission","slice"]; json.dump(dict(zip(k,sys.argv[2:])), open(sys.argv[1],"w"), indent=2)' \
  "$WATCH_DIR/slices/$name.json" "$session" "$wt_dir" "$base" "$project" "$mission" "$slice"
echo "builder: $session  worktree: $wt_dir  branch: $name  base: $base"
