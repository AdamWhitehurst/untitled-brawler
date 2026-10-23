# Shared paths for brawler-rpi scripts. Source it; it sets RIG_NAME, RIG_ROOT, REPO, WS_ROOT, WT_ROOT, WATCH_DIR.
RIG_NAME="${RIG_NAME:-brawler-rpi}"
RIG_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
REPO="$(cd "$RIG_ROOT/.." && pwd -P)"
WS_ROOT="${WORKSPACE_ROOT:-$(rig config get workspace.root)}"
WT_ROOT="${BRAWLER_WORKTREES_ROOT:-$HOME/wt}/$(basename "$REPO")"
WATCH_DIR="$WS_ROOT/exhaust/pr-watch/$RIG_NAME"

rig_id() { rig ps --json | python3 -Ic 'import json,sys; print(next(r["rigId"] for r in json.load(sys.stdin) if r["name"]==sys.argv[1]))' "$RIG_NAME"; }

project_root() {
  local id="${1:?project id required}" catalog
  catalog="$(rig config get workspace.catalog_path)"
  case "$catalog" in /*) ;; *) catalog="$WS_ROOT/$catalog" ;; esac
  python3 -I - "$catalog" "$id" <<'PY'
import pathlib, re, sys

catalog = pathlib.Path(sys.argv[1])
wanted = sys.argv[2]
selected = False
for line in catalog.read_text().splitlines():
    match = re.match(r"\s*-\s+id:\s*['\"]?([^'\"\s]+)", line)
    if match:
        selected = match.group(1) == wanted
        continue
    if selected:
        match = re.match(r"\s+root:\s*['\"]?([^'\"]+?)['\"]?\s*$", line)
        if match:
            print((catalog.parent / match.group(1)).resolve())
            break
else:
    raise SystemExit(f"project {wanted!r} has no root in {catalog}")
PY
}
