#!/usr/bin/env bash
# Usage: verify-plan.sh <project-id>
# Read-only check that a planned project is complete and renders in the Projects view. Exit 0 = PASS.
set -uo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

id="${1:?project id required}"; proj="$WS_ROOT/projects/$id"
fail=0; bad() { echo "FAIL: $*"; fail=1; }

[ -d "$proj" ] || { echo "FAIL: no project folder $proj"; exit 1; }
catalog="$(rig config get workspace.catalog_path)"; case "$catalog" in /*) ;; *) catalog="$WS_ROOT/$catalog" ;; esac
grep -Eq "^[[:space:]]*-[[:space:]]+id:[[:space:]]+$id[[:space:]]*$" "$catalog" || bad "$catalog has no entry for $id"
grep -q '^proofPolicy:' "$proj/project.yaml" && grep -q 'judges:' "$proj/project.yaml" || bad "project.yaml has no proofPolicy.judges"
grep -Eq '^intent:' "$proj/SPEC.md" || bad "project SPEC.md has no intent frontmatter"

n=0
for m in "$proj"/missions/*/; do
  [ -d "$m" ] || continue; n=$((n+1)); name="$(basename "$m")"
  [ -f "$m/mission.yaml" ] || { bad "$name: no mission.yaml (create missions with rig scope mission create)"; continue; }
  out="$(rig scope --workspace "$proj" audit --mission "$name" 2>&1)"
  if echo "$out" | grep -q '^PASS'; then :
  elif echo "$out" | grep -q '^WARN'; then echo "WARN: $name: advisory audit findings:"; echo "$out" | sed 's/^/    /'
  else bad "$name: rig scope audit has findings:"; echo "$out" | sed 's/^/    /'; fi
  for s in "$m"/slices/*/; do
    [ -d "$s" ] || continue; sn="$(basename "$s")"
    grep -q "slices/$sn/slice.yaml" "$m/mission.yaml" || bad "$name/$sn: not listed in mission.yaml"
    grep -Eq '^depends_on:[[:space:]]*\[' "$s/SPEC.md" || bad "$name/$sn: SPEC.md depends_on must be an explicit list ([] when none)"
  done
done
[ "$n" -gt 0 ] || bad "$id has no missions"
[ "$fail" = 0 ] && echo "PASS: $id ($n mission(s)); open rig tui > Projects to eyeball it" || exit 1
