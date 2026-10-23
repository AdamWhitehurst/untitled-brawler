# brawler-rpi culture

This rig develops Untitled Brawler, a multiplayer 2.5D brawler built with Rust, Bevy, Lightyear, and a custom voxel map engine.

## Guiding Rules

- OpenRig provides skills and docs via `rig context list` / `rig context get <ref>` (e.g. `skills/core/watchdog`) that explain how OpenRig and its features work and how to use them. Always do due diligence there, and in `rig <command> --help`, before guessing how to operate within OpenRig. You'll want to know how to correctly use openrig and its commands to ensure that project is properly maintained.
- All work must be done and recorded so that the Projects view of openrig accurately reflect the state of work, and that all the sections/fields of the Projects view are properly specified.
- Avoid excessive documentation. Record high-signal information that cannot otherwise be found or known that future readers should to know. Ask yourself: Would the information cause the reader to act differently or help inform decisions? If not, it likely doesn't need to be included. Low-value information is noise that pollutes context and obscures signal. This is especially true for prose that only documents steering/revision changes e.g. "Agent now does X instead of Y".
- Avoid over-prescription and explanation. State instructions or guidance plainly, and don't repeat yourself.
- Avoid unnecessary proscription. State the behavior you want. A prohibition belongs in the instruction only after that behavior has been observed and a positive instruction did not stop it. A guessed failure mode is not evidence.

## Project Management

A project's mission is a vertical slice that results in player-visible change to the game. Player-visible outcomes are the goal at every step: a slice is done when the human sees its outcome in play.

- Reply to assigned work on its queue row. When the work moves to another seat, hand it off with `rig queue handoff-and-complete <qitem> --to <seat> --summary "<one line>"`; when the next row already exists, close with `--closure-reason handed_off_to --closure-target <its qitem id>`. Do not drive another seat's prompt; `--dangerously-interact` should be avoided.
- Tag every row about a slice `project:<id>`, `mission:<mission>`, `slice:<slice>`, copied from the row you answer (workflow packets can't carry tags). `<mission>` and `<slice>` are the directory names. The Projects view counts only rows tagged this way; any other row is invisible. Never attach a slice to a row addressed to the human.

## The loop

1. The human and the kernel advisor shape the mission and write the mission `SPEC.md` in its project: its intent and the outcomes a player will see. The human approves it and hands it to `orch-lead`.
2. `rig/workflows/plan.workflow.yaml`: research, design, the human's design gate, slicing, plan review, then the human's slicing gate. Approval starts every slice whose dependencies are satisfied.
3. `rig/workflows/build.workflow.yaml`, once per slice, each slice with its own builder seat and worktree: implement, two independent code reviews, the design sync, then the human's playtest gate. Approval starts each slice this one unblocks.

The workflow specs route every step. A seat does its step and closes it; it never routes around a gate.

## Where things live

All planning artifacts live in the mission and slice files. OpenRig's conventions hold their shapes: `rig context get mission-slice-sop` and `~/.openrig/reference/sdlc-conventions.md`.

- Mission `SPEC.md`: intent and outcomes as the human approved them, then the slice map the planner adds.
- Mission `DESIGN.md`: the contracts the mission adds or changes, as the human approved them at the design gate. Every seat builds to it.
- Mission `NOTES.md`: what a later seat would otherwise re-derive: research findings, each decision with the alternative it rejected and why, and plan review findings. Status lives in queue rows and workflow instances, not here.
- Slice `SPEC.md`: the outcome it makes visible, mini-requirements, proof contract. `PROGRESS.md` holds the slice's open items and its `## Design deviations`; delivery state lives in the project record. `PROOF.md` indexes the proof contract, evidence drops, and judgments; `proof/` holds the evidence.
- `docs/kb`: how the game and its code work today. The designer keeps its architecture pages, indexed from `docs/kb/tech_index.md`, true as slices land.
- Code: the slice's worktree under `~/wt/untitled-brawler/`, one commit per mini-requirement. After the playtest gate the builder submits the stack with `git review` and answers the PRs until they merge. Assigning a slice authorizes pushing that slice's branch and its `git review` PRs; `master` and other branches need the human's word, and only the human merges.

## What to write

Only record signal. A line recorded without intention to steer reader to act differently (or is already found elsewhere) is noise. Facts that git, the queue, proof drops, or the workflow trace already hold are referenced, not copied. Under-document rather than over-document: noise hides the signal and pollutes context. The same goes for guidance, briefs, docs and examples: when an instruction or example falls short, revise it before adding another; add new text only when revising can't do the job.

## The project record

OpenRig's project view reads `mission.yaml`, each `slice.yaml`, slice `SPEC.md` frontmatter, proof judgments, and queue rows tagged with the slice. `orch-peer` keeps that record true. `orch-lead` records the human's verdicts at the gates: approval stamps and judgments of play items. `review-codex` judges the automated proof items it runs.

## How seats work

- Close a step with `rig workflow project` and put the facts the next seat needs in the result note.
- A reviewer reviews at most twice per workflow instance. The second review in that instance is final: open findings ride to the human's gate. A playtest rejection starts a fresh build instance with its own review count.
- Code that has to depart from `DESIGN.md` records the deviation and its reason in the slice `PROGRESS.md`. Reviewers block an unrecorded or unjustified deviation; the human accepts or rejects the rest at the playtest gate.
- A question only the human can answer rides in the result note to the next gate. A step that cannot proceed closes `waiting` with the blocker stated. When the blocker is facts the mission research does not cover, send the questions to `research-researcher@brawler-rpi` as a queue row; its findings land in the mission `NOTES.md`.
- Exceptions route to `orch-lead`, which reaches the human.
- Something you noticed that is not yours to fix and has no row yet: `rig stream emit --source <you> --hint-type bug|idea|risk --hint-urgency routine|urgent|critical --hint-tags project:<id>,mission:<mission>,slice:<slice> --body "<what, where as path:line, why it matters>"`, plus `--hint-destination <seat>` when you know the owner. The kernel's queue-worker turns it into a row carrying those tags; `critical` shows as attention in the TUI.
- Papercuts go on the stream the same way, with `--hint-type papercut`: a tool call that failed, an instruction that was wrong or missing and had to be worked out, a step done by hand that a script could do. Say what happened and what would have avoided it.
- Enter a worktree with a standalone `cd <dir>`, then run git and cargo there, so commands match the allowlist without a prompt and cargo reads the worktree's `.cargo/config.toml`. Write paths literally.

## Asking the human

Send decisions, sign-offs, approvals, and hands-on tests to the address from `rig gateway human list --json` so they appear in NEEDS YOU:

```sh
rig queue create --destination <registered-human-address> --summary "<what you need>" \
  --body-file <brief.md> --evidence-ref <plan, PROOF.md path, or PR URL> \
  --human-intent decision --tags project:<id> --mission <mission> --verify --json
```

Tell the human exactly how to verify anything they must try.
