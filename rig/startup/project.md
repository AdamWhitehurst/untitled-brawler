# brawler-rpi operating notes

Identity: `rig whoami --json`. Your packet: `rig queue list`. The instance and its frontier: `rig workflow show <instance>`.

Close a step:

```
rig workflow project --instance <id> --current-packet <qitem> --exit handoff|done|failed|waiting \
  --actor-session <you> --result-note "<artifact path, verdict, facts the next seat needs>" \
  --evidence-ref <artifact path>
```

`failed` is the artifact's verdict. A step you cannot perform is `waiting` with `--blocked-on`.

Artifacts:

- project: `<workspace>/projects/<project>/`, where `<workspace>` is `rig config get workspace.root`
- mission: `<project>/missions/<mission>/` with `SPEC.md`, `NOTES.md`, `PROGRESS.md`
- slice: `<mission>/slices/<slice>/` with `SPEC.md`, `PROGRESS.md`, `PROOF.md`, `proof/`
- code: `~/wt/untitled-brawler/<mission>-<slice>`, branch `<mission>-<slice>`, worked by `build-<mission>-<slice>@brawler-rpi`

Scope commands take `--workspace <project path>`. Claude seats invoke skills as `/<name>`; Codex seats read the file.

Verification: `cargo test --workspace` and `cargo clippy --workspace`, one cargo command at a time.
