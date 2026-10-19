---
date: 2026-04-27T18:01:44-07:00
researcher: Adam Whitehurst
git_commit: ca3437daf740068756f02597b148c9c54ee6ca29
branch: master
repository: untitled-brawler
topic: "Editing an ability asset hard-freezes the client"
tags: [bug, ability, hot-reload, file_watcher, bevy-upstream]
status: root-cause-identified-upstream-fix-pending-release
last_updated: 2026-04-28
last_updated_by: Adam Whitehurst
---

# Bug: Editing an ability asset hard-freezes the client (and server)

## Symptom

Saving any asset under `assets/` while the client (or server) is running
hard-freezes the process — Ctrl+C is unresponsive, btop kill required.
First reproduced on `assets/abilities/punch.ability.ron`, but the cause is
not specific to ability assets.

## Root cause: bevy 0.18.1 hot-reload deadlock

`bevy_asset::server::handle_internal_asset_events`
(`bevy_asset/src/server/mod.rs:1734`) acquires the `AssetInfos` write-lock
at the top:

```rust
let mut infos = server.write_infos();   // mod.rs:1735 — holds RwLock<AssetInfos> write-guard
```

It then iterates `AssetSourceEvent`s. For `AddedAsset`, `RemovedAsset`,
`AddedFolder`, `RemovedFolder`, and `RenamedFolder` events, the closure
`reload_parent_folders` (mod.rs:1806-1817) calls
`server.load_folder_internal(...)` (mod.rs:1814). Inside
`load_folder_internal` (mod.rs:1091 / 1132 depending on build):

```rust
self.write_infos().stats.started_load_tasks += 1;
```

This re-acquires the same `RwLock` for write **on the same thread** that
already holds the outer write-guard. `std::sync::RwLock` is not re-entrant
→ same-thread writer↔writer deadlock. The `drop(infos)` at mod.rs:1923
that would have prevented this is gated
`#[cfg(any(target_arch = "wasm32", not(feature = "multi_threaded")))]`,
so on multi-threaded native builds (ours) it never runs before
`load_folder_internal`.

Why saving (not editing) triggers it: most editors save by writing a temp
file and `rename()`-ing over the target. notify-rs reports that as
`RemovedAsset(temp)` + `AddedAsset(target)` — both routed through
`reload_parent_folders`. A single in-place `ModifiedAsset` would go
through `reload_path` instead and would not deadlock.

Both client and server are affected because both enable bevy's
`file_watcher` feature and run the same `bevy_asset` code.

## Diagnostic evidence

Stack from frozen process (identical on client and server):

```
RwLock::write_contended → futex_wait
  ← AssetServer::write_infos          (server/mod.rs:163)
  ← AssetServer::load_folder_internal (server/mod.rs:1091)
  ← handle_internal_asset_events::{closure}  (server/mod.rs:1814)
  ← handle_internal_asset_events     (exclusive system, server/mod.rs:1734)
```

Captured via `/proc/$PID/task/$PID/stack` and
`gdb -batch -p $PID -ex 'thread 1' -ex 'bt'`.

All other threads (notify-rs, IO/Async/Compute task pools, tokio workers)
are normally parked — the wedge is exclusively on the main thread holding
the `AssetInfos` write-lock and waiting on it.

## Upstream status

- Bevy issue: [bevyengine/bevy#23954](https://github.com/bevyengine/bevy/issues/23954) (opened 2026-04-23)
- Bevy fix: [PR #23980](https://github.com/bevyengine/bevy/pull/23980)
  "Fix hot reloads of a folder resulting in deadlocks", merged 2026-04-26,
  commit `6090daa63`.
- The fix removes `write_infos()` from inside `load_folder_internal`,
  counts started loads at call sites without holding `infos`, and defers
  `load_folder_internal` invocations until after the outer lock is dropped.
- **Not in any released bevy version** — `release-0.18.1` was not
  back-ported. Will land in the next bevy release (presumably 0.19).

## Workaround

Until bevy ships the fix:
- **Restart the client/server after editing any asset file.**
- Alternative: disable `file_watcher` in `crates/client/Cargo.toml` and
  `crates/server/Cargo.toml` — eliminates the freeze entirely at the cost
  of any hot-reload during dev.

A local cherry-pick of #23980 onto a forked v0.18.1 `bevy_asset` patched
via `[patch.crates-io]` is feasible but was not applied — the team
elected to wait for the upstream release.

## Why initial hypotheses (H1-H4) were wrong

H1-H4 below all assumed the freeze was caused by **our** code reacting to
the asset event (rollback storm, asset-event feedback loop, reflect
recursion, sub-ability chain). Diagnostic instrumentation invalidated all
four:

- **H1 (rollback storm)** — Zero `debug_h1` traces fired post-save. No
  `Added<ActiveAbility>`, no observer fires, no new activations.
- **H2 (`reload_ability_defs` runaway)** — `reload_ability_defs` ran but
  saw zero `AssetEvent::Modified` events post-save (the asset server
  never got that far).
- **H3 (reflect deserialize recursion)** — `AbilityAssetLoader::load`
  was never entered post-save. The asset loader pipeline was upstream of
  the wedge.
- **H4 (sub-ability chain spam)** — Zero `debug_h4` traces. No sub-ability
  spawns, no input effects firing.

The unifying observation — "no user code on the asset path runs at all
post-save" — is what redirected investigation up the stack into bevy's
asset server itself.

## Code references (ours, for context)

- `crates/client/Cargo.toml:7-8`, `crates/server/Cargo.toml:17` —
  `file_watcher` enabled by default.
- `crates/protocol/src/ability/loader.rs` — `AbilityAssetLoader` (not
  involved in the freeze; never entered post-save).
- `crates/protocol/src/ability/loading.rs` — `reload_ability_defs` (runs,
  but receives no events because bevy's asset server is wedged).

## Original hypotheses (kept for trail)

### H1: Asset reload triggers an unbounded rollback / re-prediction storm
Status: **INVALIDATED** — see "Why initial hypotheses were wrong" above.

### H2: `file_watcher` fires repeated reload events that race the AbilityDefs swap
Status: **INVALIDATED** — `reload_ability_defs` sees zero events post-save.

### H3: Reflect-deserialize of the modified asset enters a recursion that does not panic
Status: **INVALIDATED** — `AbilityAssetLoader::load` is never entered post-save.

### H4: Sub-ability chain fires every frame while the modified parent is active
Status: **INVALIDATED** — no `OnInputEffects::Ability` activations post-save.
