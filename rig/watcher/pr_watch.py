#!/usr/bin/env python3
"""Deterministic GitHub PR watcher for brawler-rpi builders.

Each registered slice (written by scripts/add-builder.sh) names a builder session and worktree.
The slice's PRs are the `maiao.<Change-Id>` heads that `git review` creates for the worktree's
commits. Per tick the watcher queues:
  - the builder: new review comments, reviews and PR comments not marked as agent-written,
    and newly failing checks;
  - orch-lead: a PR closed without merging, and the slice once every one of its PRs merged.
It only reads GitHub. State lives beside the registry so each event is queued once.
"""

import argparse
import fcntl
import json
import subprocess
import sys
import time
from pathlib import Path

RIG = "brawler-rpi"
LEAD = f"orch-lead@{RIG}"
AGENT_MARK = f"<!-- {RIG}:agent -->"
FAILED = {"FAILURE", "ERROR", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED"}


def run(*cmd, cwd=None):
    return subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True).stdout


def gh_json(*args, cwd=None):
    return json.loads(run("gh", *args, cwd=cwd) or "null")


def watch_dir():
    root = run("rig", "config", "get", "workspace.root").strip()
    return Path(root) / "exhaust" / "pr-watch" / RIG


def change_ids(entry):
    """Change-Ids on the slice branch above its base."""
    out = run("git", "log", "--format=%(trailers:key=Change-Id,valueonly)",
              f"{entry['base']}..HEAD", cwd=entry["worktree"])
    return [line.strip() for line in out.splitlines() if line.strip()]


def queue(dest, entry, summary, body):
    tags = f"project:{entry['project']},mission:{entry['mission']},slice:{entry['slice']}"
    run("rig", "queue", "create", "--destination", dest, "--summary", summary,
        "--body", body, "--tags", tags, "--json")


def feedback(slug, number, details):
    """Human-written comments, reviews and inline review comments, keyed by a stable id."""
    items = []
    for c in details.get("comments") or []:
        items.append((f"c{c['id']}", c["author"]["login"], c["body"], c.get("url", "")))
    for r in details.get("reviews") or []:
        if r["body"] or r["state"] == "CHANGES_REQUESTED":
            items.append((f"r{r['id']}", r["author"]["login"], f"[{r['state']}] {r['body']}", ""))
    for c in gh_json("api", f"repos/{slug}/pulls/{number}/comments", "--paginate") or []:
        where = f"{c['path']}:{c.get('line') or c.get('original_line')}"
        items.append((f"i{c['id']}", c["user"]["login"], f"{where} {c['body']}", c["html_url"]))
    return [i for i in items if AGENT_MARK not in i[2]]


def tick_slice(name, entry, st):
    wt = entry["worktree"]
    if not Path(wt).is_dir():
        return f"{name}: worktree missing"
    st.setdefault("ids", [])
    st["ids"] = sorted(set(st["ids"]) | set(change_ids(entry)))
    slug = st.get("slug") or gh_json("repo", "view", "--json", "nameWithOwner", cwd=wt)["nameWithOwner"]
    st["slug"] = slug
    heads = {f"maiao.{i}" for i in st["ids"]}
    prs = [p for p in gh_json("pr", "list", "-R", slug, "--state", "all", "--limit", "200",
                              "--json", "number,headRefName,state,url,title")
           if p["headRefName"] in heads]
    seen = set(st.setdefault("seen", []))
    notes, checks = [], []
    for pr in prs:
        key = str(pr["number"])
        if pr["state"] == "CLOSED" and f"closed:{key}" not in seen:
            queue(LEAD, entry, f"PR closed unmerged: {pr['title']}",
                  f"{pr['url']} for slice {name} ({entry['session']}) was closed without merging.")
            seen.add(f"closed:{key}")
        if pr["state"] != "OPEN":
            continue
        details = gh_json("pr", "view", key, "-R", slug,
                          "--json", "comments,reviews,statusCheckRollup,headRefOid")
        for fid, author, body, url in feedback(slug, key, details):
            if fid not in seen:
                notes.append(f"- {pr['url']} {author}: {body.strip()[:400]} {url}".rstrip())
                seen.add(fid)
        for check in details.get("statusCheckRollup") or []:
            verdict = check.get("conclusion") or check.get("state")
            cid = f"ci:{details['headRefOid']}:{check.get('name') or check.get('context')}"
            if verdict in FAILED and cid not in seen:
                checks.append(f"- {pr['url']} {check.get('name') or check.get('context')}: "
                              f"{verdict} {check.get('detailsUrl') or check.get('targetUrl') or ''}".rstrip())
                seen.add(cid)
    if notes:
        queue(entry["session"], entry, f"PR feedback on {name}",
              "Answer each on its PR; end your replies with " + AGENT_MARK + ".\n" + "\n".join(notes))
    if checks:
        queue(entry["session"], entry, f"Failing checks on {name}", "\n".join(checks))
    merged = bool(prs) and len(prs) == len(heads) and all(p["state"] == "MERGED" for p in prs)
    if merged and "slice-merged" not in seen:
        queue(LEAD, entry, f"Slice merged: {name}",
              f"Every PR of slice {name} merged. Have orch-peer remove {entry['session']} "
              f"with scripts/remove-builder.sh {name} and close the slice record.")
        seen.add("slice-merged")
    st["seen"] = sorted(seen)
    return f"{name}: {len(prs)} PRs, {len(notes)} feedback, {len(checks)} checks{', merged' if merged else ''}"


def tick(base):
    state_path = base / "state.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {}
    lines = []
    for reg in sorted((base / "slices").glob("*.json")):
        name = reg.stem
        try:
            lines.append(tick_slice(name, json.loads(reg.read_text()), state.setdefault(name, {})))
        except (subprocess.CalledProcessError, KeyError, ValueError) as exc:
            err = getattr(exc, "stderr", "") or exc
            lines.append(f"{name}: error {str(err).strip()[:200]}")
        state_path.write_text(json.dumps(state, indent=2))
    for gone in set(state) - {p.stem for p in (base / "slices").glob("*.json")}:
        del state[gone]
    state_path.write_text(json.dumps(state, indent=2))
    print(time.strftime("%H:%M:%S"), "tick:", "; ".join(lines) or "no slices", flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--loop", action="store_true", help="tick every --interval seconds")
    ap.add_argument("--interval", type=int, default=300)
    args = ap.parse_args()
    base = watch_dir()
    (base / "slices").mkdir(parents=True, exist_ok=True)
    lock = open(base / "lock", "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        sys.exit("another pr_watch is running")
    while True:
        tick(base)
        if not args.loop:
            return
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
