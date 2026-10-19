# Shared paths for brawler-rpi scripts. Source it; it sets RIG_NAME, RIG_ROOT, REPO, WS_ROOT, WT_ROOT, WATCH_DIR.
RIG_NAME="${RIG_NAME:-brawler-rpi}"
RIG_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
REPO="$(cd "$RIG_ROOT/.." && pwd -P)"
WS_ROOT="${WORKSPACE_ROOT:-$(rig config get workspace.root)}"
WT_ROOT="${BRAWLER_WORKTREES_ROOT:-$HOME/wt}/$(basename "$REPO")"
WATCH_DIR="$WS_ROOT/exhaust/pr-watch/$RIG_NAME"

rig_id() { rig ps --json | python3 -Ic 'import json,sys; print(next(r["rigId"] for r in json.load(sys.stdin) if r["name"]==sys.argv[1]))' "$RIG_NAME"; }
