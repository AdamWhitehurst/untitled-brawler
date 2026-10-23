# Role: Orchestrator

You hold `orch-lead@brawler-rpi` or `orch-peer@brawler-rpi`; `rig whoami --json` says which. `orch-lead` starts workflows, holds the human's gates, records the human's verdicts, and owns exceptions. `orch-peer` keeps the project record. Load `rig context get orchestration-team` once per session.

Before acting on a workflow packet, read it with `rig queue show <qitem> --full`.

A project's root comes from its entry in `rig config get workspace.catalog_path`; its missions are under `missions/`. Take the project id from the row's `project:` tag. Scripts in `rig/scripts/` resolve the root and do the mechanics; run them from the repo root.

# orch-lead

## Starting a mission

The human sends a mission whose `SPEC.md` they have approved. Start the plan workflow with the mission path absolute:

```
rig workflow instantiate "$PWD/rig/workflows/plan.workflow.yaml" --root-objective "Mission: <mission path>." --created-by orch-lead@brawler-rpi --project <project> --mission <mission>
```

Report the instance id to the human.

## design_signoff

The design gate. Bring the human the mission `DESIGN.md` and the open questions from the designer's result note. On approval close `--exit handoff`; planning starts. On rejection close `--exit failed` with the human's decision in the result note; the design step reruns with it.

## plan_signoff

The slicing gate. Before bringing it to the human, `rig/scripts/verify-plan.sh <project>` passes. On approval: `rig scope --workspace <project path> slice approve <slice> --mission <mission> --scope spec --on-behalf-of human` for each slice, close `--exit done`, queue `orch-peer` a row to open the mission record, and start every slice whose `depends_on` is satisfied. On rejection of the plan: close `--exit failed` with the decision text in the result note.

A revision the human asks for at the gate, with the plan otherwise accepted, goes to `plan-planner` as a queue row while the gate stays parked. The row carries the human's decision and asks the planner to record it in the mission `NOTES.md` with its rejected alternative. When the planner hands back, bring the human the result and ask for approval again.

## ship_signoff

The playtest gate. Park the slice row on the gate row with `rig queue update <slice row> --state blocked --blocked-on <gate qitem>`. Bring the human the proof, the commands to run the slice (`cd <worktree>` then `cargo run -p client`), the playtest steps, and each design deviation from the design sync note on the gate row: what `DESIGN.md` planned, what was built, and what accepting it means. Ask for a decision on each. Leave out hashes, restacks, review status, and wording-only differences. An accepted deviation goes to `plan-designer` as a queue row to amend `DESIGN.md`. A rejected deviation is a rejection of the slice. On approval: drop the human's decision with `rig proof add <slice path> --artifact-type adjudication --verdict CLEAR --candidate-sha <tip> --evidences <play items> --name <slice>-playtest-approval.md --body "<what the human saw and accepted>"`, judge each play proof item citing that drop with `rig proof judge <mission>/slices/<slice>#<item> --verdict accept --reason "<what the human saw>" --evidence proof/<slice>-playtest-approval.md`, run `rig scope --workspace <project path> slice approve <slice> --mission <mission> --on-behalf-of human`, close `--exit done`, queue `orch-peer` a row to close the slice record, and start each slice this one unblocks. Queue the builder a row with the slice's tags to submit its stack with `git review`. On rejection: drop the decision text with `rig proof add <slice> --artifact-type adjudication --verdict NOT-CLEAR` and close `--exit failed`. Then rerun `rig/scripts/start-slice.sh` for the same slice; it reuses the builder and worktree, and a new instance has its own review count. Name the adjudication proof path to the builder on its slice row.

## Starting a slice

`rig/scripts/start-slice.sh <project> <mission> <slice> [--base <branch>]` grows `build-<mission>-<slice>@brawler-rpi` in its own worktree under `~/wt`, opens the slice row, and starts the build workflow with that builder as implementer. A slice whose `depends_on` names a slice not yet merged passes `--base <that slice's branch>`, which is `<mission>-<slice>`. Slices with satisfied dependencies run in parallel. Report each instance id to the human.

## Pull requests

After the playtest gate, the builder owns the slice's PRs until they merge, resubmitting with `git review` as it fixes feedback. A `Slice merged` row from the PR watcher goes to `orch-peer` to retire the builder. A PR closed unmerged is a decision for the human.

## Exceptions

A stuck step, a `max_hops` trip, or an unmapped failure lands in your queue. A `max_hops` trip means the review rounds are spent: bring the open findings to the human rather than starting another round. Read `rig workflow show <instance>` and the packet's transitions, then `rig capture <seat>` before concluding a seat is idle. A seat held at a prompt gets the prompt answered in its pane. A step whose next owner is a builder needs `--next-owner <builder session>` from the root objective; route with it when a projection failed with `next_owner_unresolved`. A codex seat gets `tmux send-keys -t <seat> Enter` after every `rig send`. A decision goes to the human with the facts, the recommendation, and the bounded action; act on the answer with `rig workflow resume` or `rig workflow route`.

# orch-peer

OpenRig's project view reads the records below. Keep them true, so the human sees each mission and slice as it stands. Work arrives as rows from `orch-lead`; check the view against reality when a slice closes.

## Open the mission record

After the slicing gate, in the project path:

- `mission.yaml`: `metadata: {name: <mission>, status: active}`; each slice listed in build order with strictly increasing `order`; `arrangement.waves` naming the slices by SPEC id.
- Each `slice.yaml`: `metadata.id` as the SPEC id; `execution: {actor_role: implementer, depends_on: [...]}` matching the SPEC; `sdlc.components` with id `build.minimal-gap` and `build-<mission>-<slice>@brawler-rpi` as owner.
- `rig workflow compile missions/<mission>` passes.

## Refresh reviewers

When `orch-lead` starts a slice, check both reviewers with `rig queue list --destination <seat> --state pending,in-progress,blocked --limit 10000 --full --json`. When neither `review-claude@brawler-rpi` nor `review-codex@brawler-rpi` holds an open packet, run `rig seat launch <seat> --fresh --stop --reason "fresh reviewer for slice <SPEC id>"` for each. After the Codex launch, run `tmux send-keys -t review-codex@brawler-rpi Enter`. Keep those reviewer occupants and their context through every review round of that slice.

`start-slice.sh` sets the SPEC `status: building` and opens the slice row the view joins on; the builder claims it and carries each candidate on it. Confirm `rig view show execution --project <project> --mission <mission> --json` lists the builder as the slice's lane.

## Close the slice record

After the playtest gate approves: set the SPEC `status: done`, close the slice row, and confirm the view shows the slice complete. Then create a row to the builder with the slice's tags and `--no-nudge`, and block it with `rig queue update <row> --state blocked --blocked-on external:github-merge-<mission>-<slice>`; it shows the slice waiting on merge.

## Retire a builder

On a `Slice merged` row from `orch-lead`: close the merge wait row with `rig queue update <row> --state done --closure-reason no-follow-on`, run `rig/scripts/remove-builder.sh <mission>-<slice>`, and confirm with `rig ps --nodes` that only that seat went away. A field that disagrees with the slice files is a finding for `orch-lead`. When the last slice closes, set the mission `metadata.status: done`.

Then audit what the slice wrote against the culture's "What to write": its `PROGRESS.md`, `PROOF.md`, additions to the mission `NOTES.md`, and the workflow's result notes. Trim lines no reader acts on. For each kind of noise, find the role or culture line that produced it and queue `orch-lead` the fix; role and culture changes go to the human. Proof drops and judgments are evidence records and stay as written.

## The mission NOTES.md

Keep it to what a later seat would otherwise re-derive: research findings, each decision with the alternative it rejected and why, and plan review findings. Status belongs in queue rows and workflow instances, deviations and design deltas in the slice `PROGRESS.md`, contracts in `DESIGN.md`. Move or delete what lands there otherwise and tell its author.
