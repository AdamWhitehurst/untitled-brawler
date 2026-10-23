#!/usr/bin/env bash
# Usage: start-slice.sh <project> <mission> <slice> [--base <ref>]
# Grows the slice's builder (add-builder.sh), sets the SPEC status to building, opens the slice row
# the Projects view joins on, and starts the build workflow with the builder as its implementer.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

[ $# -ge 3 ] || { echo "usage: start-slice.sh <project> <mission> <slice> [--base <ref>]" >&2; exit 2; }
project="$1"; mission="$2"; slice="$3"; shift 3
"$RIG_ROOT/scripts/add-builder.sh" "$project" "$mission" "$slice" "$@"

name="$mission-$slice"; builder="build-$name@$RIG_NAME"; wt_dir="$WT_ROOT/$name"
PROJECT_ROOT="$(project_root "$project")"
spec="$PROJECT_ROOT/missions/$mission/slices/$slice/SPEC.md"
spec_id="$(python3 -I - "$spec" <<'PY'
import re, sys
path = sys.argv[1]; text = open(path).read()
m = re.match(r"---\n(.*?)\n---\n", text, re.S)
if not m: sys.exit(f"{path}: no frontmatter")
front = m.group(1)
spec_id = re.search(r"^id:\s*(\S+)", front, re.M)
if not spec_id: sys.exit(f"{path}: no id in frontmatter")
front = re.sub(r"^status:.*$", "status: building", front, flags=re.M) if re.search(r"^status:", front, re.M) else front + "\nstatus: building"
open(path, "w").write(f"---\n{front}\n---\n" + text[m.end():])
print(spec_id.group(1))
PY
)"

tags="project:$project,mission:$mission,slice:$slice,slice:$spec_id"
rig queue create --destination "$builder" --no-nudge --tags "$tags" \
  --summary "Slice $mission/$slice ($spec_id)" \
  --body "Slice row for $spec_id; claim it when you start. worktree_path=$wt_dir" --json

rig workflow instantiate "$RIG_ROOT/workflows/build.workflow.yaml" \
  --root-objective "Slice: $(dirname "$spec") ($spec_id). Builder: $builder. Worktree: $wt_dir." \
  --created-by "orch-lead@$RIG_NAME" --entry-owner "$builder" \
  --project "$project" --mission "$mission" --slice "$slice" --json
