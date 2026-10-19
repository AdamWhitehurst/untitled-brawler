#!/usr/bin/env bash
# Usage: new-project.sh <project-id> "<one-line intent>"
# Creates projects/<id>/ (project.yaml, SPEC.md) in the default workspace and adds it to the catalog.
# Leaves an existing folder or catalog entry alone.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

id="${1:?project id required (kebab-case)}"; intent="${2:?one-line intent required}"
[[ "$id" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] || { echo "project id must be kebab-case: $id" >&2; exit 2; }
[[ "$intent" != *$'\n'* && "$intent" != *'"'* ]] || { echo "intent must be one line without double quotes" >&2; exit 2; }

catalog="$(rig config get workspace.catalog_path)"; case "$catalog" in /*) ;; *) catalog="$WS_ROOT/$catalog" ;; esac
proj="$WS_ROOT/projects/$id"
[ -f "$catalog" ] || { echo "no project catalog at $catalog" >&2; exit 1; }

if [ -d "$proj" ]; then echo "project folder exists, leaving it: $proj"; else
  mkdir -p "$proj/missions"
  cat > "$proj/project.yaml" <<YAML
schema: openrig.project/v0alpha1
kind: project
metadata:
  id: $id
proofPolicy:
  judges:
    - review-codex@$RIG_NAME
    - orch-lead@$RIG_NAME
install:
  intent: SPEC.md
  context: []
  skills: []
missions:
  root: missions
YAML
  printf -- '---\nintent: "%s"\n---\n\n# %s\n\nRepo: `%s`\n' "$intent" "$id" "$REPO" > "$proj/SPEC.md"
  echo "created $proj"
fi

if grep -Eq "^[[:space:]]*-[[:space:]]+id:[[:space:]]+$id[[:space:]]*$" "$catalog"; then
  echo "catalog already lists $id"
else
  grep -Eq '^projects:[[:space:]]*$' "$catalog" || { echo "catalog $catalog is not block-style 'projects:'; add id: $id, root: projects/$id by hand" >&2; exit 1; }
  [ -z "$(tail -c1 "$catalog")" ] || echo >> "$catalog"
  printf '  - id: %s\n    root: projects/%s\n    rigs: [%s]\n' "$id" "$id" "$RIG_NAME" >> "$catalog"
  echo "added catalog entry for $id"
fi
echo "project root: $proj"
