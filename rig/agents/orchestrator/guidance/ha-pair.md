# Orchestration pair

The orchestration pod is a high availability pair: `orch-lead` on Claude Code, `orch-peer` on Codex. The rig keeps its coordinator when one seat compacts or restarts. This is a coordination pattern, not lossless restore. A returning seat gets back what was written down plus what its peer can tell it.

Principles:

- The record is the shared memory. If the pair needs it after a compaction, it is in a queue row, the workflow instance, or the mission's NOTES.md before it is said in chat.
- The peer is a witness, not a backup copy. A returning seat rebuilds from the record first, then asks its peer what the record does not show, and treats the answer as testimony.
- Neither seat compacts, relaunches, or hands over the other. Those are the operator seat's verbs, used on the human's word.

Division of duties follows the `orchestration-team` skill's lead and peer split. In this rig the lead starts slices and owns exceptions; the peer watches coverage across the worker seats and keeps the record honest.
