# Filesystem persistence: plan

Date: 2026-10-22. Status: decided, not started. Owner: hand to the implementing team.

Sources. The architecture review at `docs/reviews/2026-10-21-filesystem-persistence-architecture.html` (artifact https://claude.ai/artifact/8WZsAuntXpHZoX9cG35mFZ) and the Map Instance review at https://claude.ai/artifact/KJ3q3eYoMVga2VPHgPQ79x. Terms are defined in `GLOSSARY.md`.

This document records what was decided and why. It names the shape of each module's interface. It does not prescribe the implementation. Where it says "about", the team picks the number.

## Why

The save directory of a Map Instance is one thing with a fixed layout. Today it is modelled as eleven generic key-value stores, each with its own file name, its own copy of the atomic write and its own version check, plus a component pair per value type to run them on the task pool. Nothing varies behind that seam. Every adapter is the filesystem and no in-memory fake exists. The glue around it holds nine latent bugs, two of which lose edits.

## Done when

- One module owns the Save Directory layout and codec.
- One Save Queue per map carries every write and delete, in order per Save Key.
- Memory is the source of truth for publish bookkeeping; disk follows it.
- A publish Revision is durable only when its whole Save Batch has landed.
- The behaviour suite in step 0 passes, including the scenarios that fail today.
- `bevy-persistence`'s synchronous half (`Store`, `StoreBackend`, `PendingStoreOps`, `FileSystemStore`) has no server or engine consumer.

## Decisions

Each decision was put to the project owner with alternatives. The reason is the one that settled it.

D1. Memory is the source of truth for singleton state; disk is write-behind. Change Set, Accepted Head, Local Head and last-saved Map Meta live on the map entity and are loaded once at open. Every write goes through the Save Queue. Reason: it is the only arrangement where one non-blocking write path is safe. Today three systems do load-mutate-save against disk every cycle, so an async write would race the next read.

D2. A publish Revision is durable when every write in its Save Batch has landed. Meta, map entities, each Chunk, each chunk-entity list, each delete, and the Publish Draft file are all members. The Local Head advances on batch completion and not before. Reason: it is the only rule under which "Local Head says N" implies "disk holds N", which the restart path relies on. Today durability means "the meta file landed".

D3. Writes are FIFO per Save Key, one in flight per key, and queued jobs for the same key coalesce. A coalesced job is credited to every batch it replaced. Reason: every adapter writes to a fixed `.bin.tmp` per key, so two concurrent writes to one key corrupt each other. Per-key ordering closes that and preserves write order. Coalescing is about ten lines on top and matches the rapid-edit pattern.

D4. Deletes are queue jobs. A tombstone is `Delete(key)` in the batch, not a cancel method. Reason: per-key FIFO makes a queued delete land after any in-flight write for that key, which is the ordering the current queue filter fails to guarantee.

D5. One Publish State component per map, holding Change Set, Accepted Head, Local Head, last-saved Map Meta and drafts awaiting their batch. Not one component per record, and not a resource. Reason: the three systems that mutate these today mutate them together, so the invariants between them get one home. A resource was rejected because a draft belongs to one map. ECS performance was considered and found irrelevant at one to a few map entities with no change-detection consumers.

D6. Batch ids are process-local inside the Save Queue. Publish Drafts are named on disk by their local revision number and deleted once the Publish Journal has an entry for that revision. `SaveOpId` and `SaveOpIdAllocator` go. Reason: one number served two lifetimes. The allocator restarts at zero each run and draft files are never deleted, so every run's first draft overwrites `unpublished/1.bin`.

D7. A failed batch is logged at `error!`, its draft is dropped, and the map is marked dirty again so the next debounce rebuilds and retries. No panic, no in-queue retry. Reason: the Change Set is only cleared when the remote accepts, so the retry is free, and a game server should keep serving players through a failed save.

D8. Preflight reads Map Meta from the resolved path, synchronously, in one stage. A map entity gets its persistence components once, after the remote-or-filesystem decision, from one constructor, pointed at the final directory. Reason: it deletes two preflight stages, the second-entity ops, the reinstall function and the duplicated insert list, and it means a map entity either has no persistence state or has correct persistence state.

D9. Shutdown runs the same save cycle the debounce runs, unconditionally, then flushes the queue blocking. Reason: today shutdown is a second copy of the cycle that skips the Change Set, so an edit in the final debounce window lands on disk but never publishes.

D10. Delivery is three shippable steps: Save Directory, then Save Queue, then Publish State and the single save cycle. Reason: the dependency runs in that order and each step deletes glue the next would otherwise migrate.

D11. A behaviour suite at the disk seam is written before step one and is the acceptance criterion for every step. Reason: the existing 52 persistence tests call the store types directly and break in step one. Tests that name a store, a queue or a component cannot be the regression net for a change that replaces all three.

D12. Vocabulary is Save Directory, Save Queue, Save Key, Save Batch, Publish State. Code names `MapSaveDir`, `MapSaveQueue`, `SaveKey`, `BatchId`, `PublishState`.

## Target shape

Interfaces only. Private fields and helper functions are the team's call.

### Save Directory

```rust
pub struct MapSaveDir { /* canonical root, active root */ }

impl MapSaveDir {
    /// Opens the Save Directory for one Map Instance. Cleans up `staging/`,
    /// validates the active pointer, quarantines and rolls back an invalid one,
    /// then resolves the active revision root. There is no other constructor.
    pub fn open(canonical: &Path, map_id: &MapInstanceId, quarantine: &QuarantineConfig) -> Self;

    /// The Map Meta: seed, generation version and spawn points. Content, so it
    /// lives under the active revision root and a restore replaces it.
    pub fn meta(&self) -> Slot<MapMeta>;
    /// Entities that belong to the map rather than to a Chunk. Content, active
    /// revision root.
    pub fn map_entities(&self) -> Slot<Vec<SavedEntity>>;
    /// The Change Set: everything edited since the Accepted Head and waiting to
    /// publish. Bookkeeping, so it lives under the canonical root and a restore
    /// does not move it. The heads and drafts below live there for the same reason.
    pub fn change_set(&self) -> Slot<MapChangeSet>;
    /// The Accepted Head: the latest Revision the remote has accepted. Canonical root.
    pub fn accepted_head(&self) -> Slot<MapRevision>;
    /// The Local Head: the latest Revision made durable here, which may be ahead
    /// of the Accepted Head. Canonical root.
    pub fn local_head(&self) -> Slot<LocalMapHead>;
    /// The Publish Journal: every manifest publish attempt and its outcome.
    /// Overworld only. Canonical root.
    pub fn journal(&self) -> Slot<RemotePublishJournal>;
    /// The Publish Draft for one local revision number: the payloads needed to
    /// publish that Revision after a restart without rereading the directory.
    /// The number is the file name, so two runs cannot write the same draft
    /// file. Canonical root.
    pub fn draft(&self, revision: u64) -> Slot<ServerMapPublishDraft>;
    /// Revision numbers that still have a draft file. Startup re-queues the
    /// ones the Publish Journal does not record yet and deletes the rest.
    pub fn drafts(&self) -> Vec<u64>;
    /// The Chunk Envelope at one position: voxel data plus the format version
    /// and chunk size it was saved under. Content, active revision root, zstd codec.
    pub fn chunk(&self, pos: IVec3) -> Slot<ChunkFileEnvelope>;
    /// The world-object spawns for one Chunk. Content, active revision root,
    /// zstd codec.
    pub fn chunk_entities(&self, pos: IVec3) -> Slot<Vec<WorldObjectSpawn>>;

    /// Creates a directory under `staging/` for one incoming Revision and
    /// returns it as a `MapSaveDir`, so restore writes through the same slots.
    /// Readers see nothing until `promote`.
    pub fn stage(&self, revision: &MapRevision) -> Result<MapSaveDir, PersistenceError>;
    /// Validates the staged directory, renames it into `revisions/`, writes the
    /// active pointer and points this directory's active root at it. Leaves the
    /// previous revision directory in place.
    pub fn promote(&mut self, staged: MapSaveDir, revision: &MapRevision) -> Result<(), PersistenceError>;
}

/// One file in the Save Directory: its path, which root it sits under, its
/// codec and its format version. Nothing else touches bytes.
pub struct Slot<T> { /* path, root kind, codec: bincode or zstd+bincode, version */ }
impl<T> Slot<T> {
    /// Reads and decodes the file. Returns `None` for an absent file and an
    /// error for one that exists but will not decode or has another format version.
    pub fn read(&self) -> Result<Option<T>, PersistenceError>;
    /// Writes atomically: encodes the value into a uniquely named temp file in
    /// the same directory, then renames it over the target. Creates parent
    /// directories. Two concurrent writes to one key cannot corrupt each other.
    pub fn write(&self, value: &T) -> Result<(), PersistenceError>;
    /// Removes the file. An absent file is not an error.
    pub fn delete(&self) -> Result<(), PersistenceError>;
}
```

One layout table, one atomic write, one versioned codec. Heads, Change Set and drafts are rooted at the canonical directory. Content is rooted at the active revision directory. Staging and promotion keep the logic that exists today in `server/src/persistence/mod.rs:565-1060`; it moves, it is not rewritten. `ChunkFileEnvelope::{encode, decode}` stays public because the wire uses it.

### Save Queue

```rust
#[derive(Component)]
pub struct MapSaveQueue { /* queued jobs, in-flight set, batches, task list */ }

pub enum SaveKey {
    Meta, MapEntities, ChangeSet, AcceptedHead, LocalHead, Journal,
    Draft(u64), Chunk(IVec3), ChunkEntities(IVec3),
}

pub enum SaveJob {
    Write { key: SaveKey, produce: Box<dyn FnOnce() -> Bytes + Send> },
    Delete { key: SaveKey },
}

pub struct BatchId(u64);

impl MapSaveQueue {
    pub fn open_batch(&mut self) -> BatchId;
    pub fn enqueue(&mut self, batch: Option<BatchId>, job: SaveJob);
    /// Spawns up to the per-frame cap, polls tasks, returns completed and failed batches.
    pub fn poll(&mut self, dir: &MapSaveDir) -> Vec<BatchOutcome>;
    /// Drains queued and in-flight work. Shutdown only.
    pub fn flush_blocking(&mut self, dir: &MapSaveDir) -> Vec<BatchOutcome>;
}

pub enum BatchOutcome {
    Completed(BatchId),
    Failed { batch: BatchId, failures: Vec<(SaveKey, PersistenceError)> },
}
```

Rules inside the queue. FIFO per key. One in-flight job per key. A queued job for a key is replaced by a later queued job for the same key, and the later job is credited to both batches. A batch completes when every job credited to it has landed. Jobs with no batch complete silently. One drain system per map calls `poll` and emits one message per `BatchOutcome`. The per-frame spawn cap stays at about 16.

### Publish State

```rust
#[derive(Component)]
pub struct PublishState {
    change_set: MapChangeSet,
    accepted_head: Option<MapRevision>,
    local_head: Option<LocalMapHead>,
    last_saved_meta: Option<MapMeta>,
    drafts_awaiting_batch: HashMap<BatchId, (u64, ServerMapPublishDraft)>,
}

impl PublishState {
    pub fn load(dir: &MapSaveDir) -> Self;
    /// Extends the change set. Returns whether anything real changed.
    pub fn record_edits(&mut self, content_dirty: &HashSet<IVec3>, chunk_entities_dirty: &HashSet<IVec3>, entities_dirty: bool, meta: &MapMeta) -> bool;
    pub fn open_draft(&mut self, batch: BatchId, revision: u64, draft: ServerMapPublishDraft);
    /// Advances the local head. Returns the draft to queue for remote publish.
    pub fn commit_batch(&mut self, batch: BatchId) -> Option<ServerMapPublishDraft>;
    /// Drops the draft. The caller re-dirties the map.
    pub fn abandon_batch(&mut self, batch: BatchId);
    /// Clears accepted candidates from the change set, sets the accepted head.
    pub fn accept_revision(&mut self, revision: &MapRevision, accepted: &MapChangeSet);
}
```

### One save cycle

A function, not a system, taking the map's components and the dirty state. It records edits into Publish State, opens a batch, enqueues Change Set, Meta, map entities, dirty Chunks, dirty chunk entities and any deletes, and for a publishable Overworld with real edits builds the draft, enqueues its file and registers it with the batch. The debounce system calls it when its timer elapses. The `AppExit` handler calls it for every map and then calls `flush_blocking`. Batch outcomes feed `commit_batch` or `abandon_batch`; a commit pushes the draft to the remote publish delta queue.

### Preflight

One stage reads Map Meta through `MapSaveDir::open(...).meta().read()`. The remote decision proceeds as today. After the decision, and after any restore has been materialised and promoted, one constructor builds `MapSaveDir`, `MapSaveQueue`, `PublishState` and, for the Overworld, the journal and delta queue, and inserts them in one command. The placeholder entity carries none of these until then.

## Delivery

### Step 0. Behaviour suite

Integration tests under `crates/server/tests/`, one file per scenario group. Each runs a real `App` with `MinimalPlugins`, the engine plugin and the server map plugin over a `tempdir`. Tests drive the server through edits, evictions, debounce ticks, `AppExit` and a fresh `App` over the same directory. They assert on files and on what the fresh server loads. They do not name a store, a queue or a component.

A golden save directory is checked in under `crates/server/tests/fixtures/golden-map/`, written by today's code with one edited Chunk, one chunk-entity list, Map Meta, map entities, a Change Set, both heads and a Publish Journal.

| # | Scenario | Fails today |
|---|---|---|
| 1 | Round trip. Edit, tick past debounce, exit, reload. Same voxels, meta, map entities, chunk entities. Variants for an evicted column and for an edit inside the final window before exit. | final-window variant |
| 2 | Golden load. The fixture loads to a known world. | no |
| 3 | Publish durability. Local Head advances exactly when every file of the revision is on disk. A draft file exists for that revision. Restart re-queues an un-journaled draft and leaves a journaled one alone. Draft names never collide across restarts. | last two |
| 4 | Tombstones. Revert a Chunk to generated while a write for it is in flight. The file ends absent and the draft marks it tombstoned. | yes |
| 5 | Concurrency. Many rapid edits to one Chunk. The file always decodes. | yes, when the race is hit |
| 6 | Failure. Directory made read-only mid-session. Server keeps serving, logs, and the edits land after permission is restored. Corrupt meta blocks the map; corrupt map entities warns and loads none. | retry part |
| 7 | Remote restore. A validated save materialises to a promoted revision, the pointer flips, heads land, the next load reads from the revision. | no |
| 8 | Shutdown. Everything queued or in flight at `AppExit` is on disk afterwards. | queued case |

Failing scenarios are written with `#[ignore = "bug N, fixed in step M"]` and enabled in the step that fixes them. Scenarios 1, 2 and 8 are the floor if the suite must shrink.

Error policies in scenario 6 are today's, preserved. Tightening them is a separate change.

### Step 1. Save Directory

Replace the nine `Fs*Store` types, `chunk_file_path`, `entity_file_path`, `atomic_save_bincode`, `load_optional_bincode` and the pointer helpers with `MapSaveDir` and `Slot`. Callers keep their load-mutate-save shape and call `dir.slot().read()` and `.write()`. Move staging and promotion behind `stage` and `promote`. Delete `install_active_revision_store_backends` in favour of re-pointing the one `MapSaveDir`. Merge the two `EntityFileEnvelope` types.

Deletes outright: `bootstrap_filesystem_revision`, `delete_materialized_chunk`, `delete_materialized_chunk_entities`, `FileSystemStore`.

Fixes bug 5 (drafts rooted at the wrong directory).

Done when the suite passes with scenarios marked for later steps still ignored, and `cargo check-all` and `cargo test-all` pass.

### Step 2. Save Queue

Replace every `StoreBackend` and `PendingStoreOps` on the server and in the engine with `MapSaveQueue`. The engine's `PendingSaves` and `drain_pending_saves` become `enqueue` calls and one drain system. `ChunkSaveCompleted` and `ChunkSaveFailed` are replaced by batch outcome messages. `save_dirty_chunks_flush`, `log_save_errors`, `forward_map_meta_save_completions`, `poll_map_entities`' save half, `poll_chunk_entity_ops` and `save_all_chunk_entities_on_exit` go. Tombstones become `Delete` jobs.

Fixes bugs 1, 2, 3, 4 and 6.

Done when scenarios 4, 5 and 8 are enabled and pass.

### Step 3. Publish State and one save cycle

Add `PublishState`. Replace every disk load of Change Set, heads and last-saved meta with the component. Name drafts by revision; delete them once journaled. Replace `PendingPublishBySaveId`, `MapPayloadSaveKey`, `normalize_chunk_save_completions` and `handle_completed_map_payload_save_for_publish` with batch commit and abandon. Fold the preflight meta stages and the map-entities load state into the single synchronous open. Make shutdown call the save cycle then `flush_blocking`.

Fixes bugs 7, 8 and 9.

Done when every scenario is enabled and passes, and the synchronous half of `bevy-persistence` has no consumer outside the client.

## Latent bugs

| # | Where | What | Fixed in |
|---|---|---|---|
| 1 | `server/src/map/mod.rs:873-892` | Shutdown flush never drains `PendingSaves.queue`. Evicted or capped chunks are lost on exit. | 2 |
| 2 | `server/src/map/mod.rs:924` | In-flight chunk tasks are not flushed on exit when `is_dirty` is false. | 2 |
| 3 | `server/src/map/mod.rs:661-674`, `homebase_publication.rs:316` | Tombstone cancel filters the queue only. An in-flight write recreates the deleted file. Homebase does not filter at all. | 2 |
| 4 | `server/src/map/mod.rs:68-81` | `log_save_errors` drops every completion without an id, which is every chunk and chunk-entity completion. `ChunkSaveFailed` is logged twice. | 2 |
| 5 | `server/src/map/mod.rs:262-276` | Drafts are rooted at the active revision directory; heads and Change Set at the canonical one. A restore moves where pending drafts live. | 1 |
| 6 | every `Fs*Store::save` | Fixed `.bin.tmp` per key. Two in-flight writes to one key corrupt each other. | 2 |
| 7 | `server/src/map/mod.rs:1108`, `server/src/persistence/mod.rs:414-475` | Save id allocator restarts at zero each run; draft files are never deleted. Each run's first draft overwrites `unpublished/1.bin`. | 3 |
| 8 | `server/src/map/mod.rs:894-955` | Shutdown writes content but not the Change Set. An edit in the final debounce window never publishes. | 3 |
| 9 | `server/src/map/remote_publish.rs:825` | The first completion with a given save id consumes the draft whatever its key. Durability means "meta landed". | 3 |

## Out of scope

- The Map Instance's own state (its candidates 1, 2, 4, 5 in the Map Instance review). Its candidate 3, `flush_dirty`, composes with this plan: the instance produces `(pos, ChunkData)` jobs and the Save Queue takes them.
- The `AsyncStore` seam and the Blossom and Nostr adapters. Three adapters and two fakes make it a real seam.
- The publish journal state machine in `remote_publish.rs`, beyond replacing its completion inputs.
- Whether to inline the remaining `bevy-persistence` async half and the client identity store. Optional follow-up.
- Error policy changes. Preserved as today.

## For the team to decide

- The exact per-frame spawn cap and whether it is per map or global.
- Whether `Slot` is one generic type or a small enum of codecs.
- How the golden fixture is regenerated when the on-disk format version bumps.
- Whether scenario 6's read-only test is portable to CI, or guarded.

## Verification

Each step ends with `cargo check-all` and `cargo test-all`, and a manual run with `cargo server-log` through an edit, an exit and a restart. Never run two cargo builds at once.
