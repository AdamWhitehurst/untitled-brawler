# Implementation Plan — Unified Map Publish Layer (Tier 2 + chunk deletion)

## Overview

Make Overworld and Homebase publish through one shared delta assembler sourced from a durable
edit change-set, with chunk deletion expressed as `Tombstoned` slots and detected by an
auto equals-generated compare. Supersedes the Phase 5 homebase full-snapshot publish path.

Design: `design-publish-unification.md`. Build behind the existing `SERVER_MAP_REMOTE_PUBLISH`
gate. Restore/import side is unchanged (already folds chained deltas to genesis).

## Global Implementation Rules

- Before any `cargo build`/`check`/`test`, verify none is running:

```bash
if pgrep -af 'cargo (build|check|test|make)|cargo-make|rustc' | grep -v pgrep; then
  echo 'A Rust build/check/test is already running; wait or stop it first.' >&2
  exit 1
fi
```

- Keep `voxel_map_engine` free of Nostr/Blossom/protocol types.
- Reuse `Store`/`AsyncStore` + `PendingStoreOps`; no new persistence traits.
- `trace!` before every expected early-out; `panic!`/`expect`/explicit rejection for impossible state.
- One commit per phase. Strip the existing `// DEBUG` lines (see handoff.md) before the first commit.

## Naming/interpretation notes (not design deviations)

- The design's `chunk_candidates` field role is kept on the engine struct under the **existing
  name `content_dirty_chunks`** (already means "genuine edits") to minimize churn; the durable
  mirror is server-side. The engine still owns the in-memory set (it is a plain `HashSet<IVec3>`).
- The design's `build_publish_delta` is realized as the **existing `upload_publish_slot`
  per-slot helper plus a new `finalize_manifest`** (sort + descriptor_root + assemble) — same
  outcome (shared assembly, bespoke homebase loop deleted), less surface area.

---

## Phase 1: Engine primitives (equals-generated + chunk delete)

### Changes

#### 1. Equals-generated compare
**File**: `crates/voxel_map_engine/src/instance.rs`
**Action**: modify (add method)

Add a method that compares a loaded chunk's current voxels to freshly-generated terrain.
`generate_terrain` returns the padded volume (`Vec<WorldVoxel>` of `shape.size()`); `ChunkData`
stores the padded voxels (see `set_voxel` padded indexing, instance.rs:120-141).

```rust
/// Returns whether the loaded chunk at `chunk_pos` is byte-identical to freshly-generated
/// terrain (i.e. holds no genuine edits). Used to classify a publish candidate as a delete
/// (equals generated -> Tombstone) vs an edit (differs -> Present).
///
/// Returns `false` when the chunk is not loaded (cannot prove it equals generated).
pub fn chunk_matches_generated(
    &self,
    chunk_pos: IVec3,
    generator: &dyn VoxelGeneratorImpl,
) -> bool {
    let Some(chunk_data) = self.get_chunk_data(chunk_pos) else {
        trace!(?chunk_pos, "chunk not loaded; cannot compare to generated");
        return false;
    };
    let generated = generator.generate_terrain(chunk_pos);
    chunk_data.matches_voxels(&generated)
}
```

Add a `ChunkData::matches_voxels(&self, voxels: &[WorldVoxel]) -> bool` accessor next to the
existing `voxels.set(...)` usage (instance.rs `set_voxel`) — compares the chunk's padded voxel
buffer element-wise to `voxels` (lengths must match `shape.size()`; mismatch returns `false`
with a `trace!`). Use the existing `ChunkData.voxels` representation; if it is palette-encoded,
compare via the same `get`/iteration API used elsewhere in `instance.rs`.

#### 2. Chunk delete store op
**File**: `crates/voxel_map_engine/src/persistence/fs_chunk.rs`
**Action**: modify (add method)

`FsChunkStore` currently has only `save`/`load` (fs_chunk.rs:21-71). Add a `delete` consistent
with `chunk_file_path` (imported from `super`, fs_chunk.rs:9):

```rust
impl FsChunkStore {
    /// Removes a chunk's on-disk file so the chunk regenerates from seed on next load.
    /// Absent file is a no-op (the desired end state already holds).
    pub fn delete(&self, key: &IVec3) -> Result<(), PersistenceError> {
        let path = chunk_file_path(&self.map_dir, *key);
        if !path.exists() {
            trace!(?path, "chunk file already absent; delete is a no-op");
            return Ok(());
        }
        std::fs::remove_file(&path)
            .map_err(|e| PersistenceError::Serialize(format!("delete chunk {key}: {e}")))
    }
}
```

### Verification
#### Automated
- [x] pgrep guard passes
- [x] `cargo test -p voxel_map_engine chunk_matches_generated` — add tests: generated chunk matches (`true`); after `set_voxel` differs (`false`); unloaded chunk (`false`)
- [x] `cargo test -p voxel_map_engine fs_chunk` — add test: `save` then `delete` removes file; `delete` on absent path is `Ok`
- [x] `cargo check-all`

#### Manual
- [x] none (pure unit-tested primitives)

---

## Phase 2: Shared manifest finalizer + overworld refactor (behavior-preserving)

### Changes

#### 1. `finalize_manifest` helper
**File**: `crates/nostr_map_persistence/src/payloads.rs` (or `manifest.rs`)
**Action**: modify (add fn)

Extract the duplicated sort + descriptor_root + manifest assembly into one helper both publish
paths call:

```rust
/// Sorts payload descriptors deterministically, computes the descriptor root, and assembles the
/// (unsigned) manifest. Shared by overworld server publish and homebase publish so both produce
/// byte-identical descriptor roots from the same slots.
#[allow(clippy::too_many_arguments)]
pub fn finalize_manifest(
    mut payloads: Vec<ManifestPayloadDescriptor>,
    map_id: MapInstanceId,
    owner: NostrPublicKey,
    revision: u64,
    previous_hash: Option<ManifestHash>,
    homebase_attestation: Option<HomebasePublicationAttestation>,
) -> Result<NostrMapManifest, MapPersistenceRejection> {
    payloads.sort_by_key(manifest_payload_descriptor_order);
    let descriptor_root = compute_descriptor_root(&payloads)
        .map_err(|e| MapPersistenceRejection::Invalid(e.to_string()))?;
    Ok(NostrMapManifest {
        map_id,
        owner,
        revision,
        previous_hash,
        payloads,
        schema_version: MAP_MANIFEST_SCHEMA_VERSION,
        descriptor_root,
        homebase_attestation,
    })
}
```

#### 2. Overworld uses the finalizer
**File**: `crates/server/src/map/remote_publish.rs`
**Action**: modify

In `prepare_server_map_publish_entry` (remote_publish.rs:294-399), replace the inline
`payloads.sort_by_key(...)` + `compute_descriptor_root(...)` + `NostrMapManifest { ... }` block
(remote_publish.rs:363-375) with a `finalize_manifest(payloads, map_id.clone(), owner,
draft.local_revision_number, previous_remote_manifest_hash, None)` call. The per-slot
`upload_publish_slot` calls (remote_publish.rs:306-361) are unchanged.

### Verification
#### Automated
- [x] pgrep guard passes
- [x] `cargo test -p nostr_map_persistence` — add a `finalize_manifest` test (descriptor_root matches a hand-built manifest)
- [x] `cargo test -p server world_persistence` — existing overworld publish tests still pass (byte-identical manifest)
- [x] `cargo check-all`

#### Manual
- [x] none (refactor; covered by existing overworld publish tests)

---

## Phase 3: Protocol — scope tombstone fields + publish confirmation message

### Changes

#### 1. Extend `HomebasePayloadScope`
**File**: `crates/protocol/src/map/homebase_publication.rs`
**Action**: modify

```rust
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct HomebasePayloadScope {
    pub edited_chunks: Vec<IVec3>,              // renamed from terrain_chunks
    pub tombstoned_chunks: Vec<IVec3>,          // NEW
    pub chunk_entities: Vec<IVec3>,
    pub tombstoned_chunk_entities: Vec<IVec3>,  // NEW
    pub includes_meta: bool,
    pub includes_map_entities: bool,
}
```

Wire-format note: bincode is positional and the attestation is produced fresh each publish (not
persisted), so no migration is needed. `MAP_MANIFEST_SCHEMA_VERSION` does not bump (scope has no
version field and `descriptor_root` is unaffected by scope contents).

Update the two non-test references to the renamed field:
- `crates/server/src/map/homebase_publication.rs:313` `scope.terrain_chunks.push(...)` →
  `scope.edited_chunks.push(...)` (further changed in Phase 5).
- test `crates/server/src/map/homebase_publication.rs:769`
  `state.payload_scope.terrain_chunks.is_empty()` → `edited_chunks` (updated in Phase 5).

#### 2. Add publish-confirmation message
**File**: `crates/protocol/src/map/mod.rs` (or wherever `HomebaseAttestationRequest`/`Response` live)
**Action**: modify

Add a C2S message and carry the new `MapRevision` to the client in the granted response so it
can be echoed back.

```rust
/// Client -> server: the player published the granted homebase manifest event to relays.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct HomebasePublished {
    pub manifest_hash: ManifestHash, // [u8; 32]
}
```

Change `HomebaseAttestationResponse::Granted` to also carry the manifest hash/revision:

```rust
Granted {
    unsigned_manifest_json: String,
    manifest_hash: ManifestHash,   // NEW: identifies the in-flight revision for confirmation
}
```

Register `HomebasePublished` on `MapChannel` (mirror `HomebaseAttestationRequest` registration).

### Verification
#### Automated
- [x] pgrep guard passes
- [x] `cargo check-all` (protocol message registration compiles client + server)
- [x] `cargo test -p protocol`

#### Manual
- [x] none

---

## Phase 4: Durable change-set store + population

### Changes

#### 1. `MapChangeSet` + store
**File**: `crates/server/src/persistence/mod.rs`
**Action**: modify

```rust
/// Durable set of chunk keys edited since the accepted-head revision, plus meta/entity change
/// flags. The publish candidate set; survives restart so prior-session edits still publish.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MapChangeSet {
    pub chunk_candidates: HashSet<IVec3>,
    pub meta_changed: bool,
    pub map_entities_changed: bool,
}

#[derive(Clone, Debug)]
pub struct FsMapChangeSetStore {
    pub map_dir: Arc<PathBuf>,
}

impl Store<(), MapChangeSet> for FsMapChangeSetStore {
    fn load(&self, _key: &()) -> Result<Option<MapChangeSet>, PersistenceError> { /* read change_set.bin; Ok(None) if absent; mirror FsAcceptedMapHeadStore::load */ }
    fn save(&self, _key: &(), value: &MapChangeSet) -> Result<(), PersistenceError> { /* atomic tmp+rename; mirror FsAcceptedMapHeadStore::save */ }
}

impl FsMapChangeSetStore {
    pub fn path(&self) -> PathBuf { self.map_dir.join("change_set.bin") }
}
```

Mirror `FsAcceptedMapHeadStore` (mod.rs:292-329) exactly, including the top-level-`save_dir`
rooting used in `materialize_validated_map_save` (mod.rs:782).

#### 2. Install backend + load on map setup
**File**: `crates/server/src/persistence/mod.rs` (`install_active_revision_store_backends`, ~mod.rs:884) and `crates/server/src/map/preparation.rs` (homebase placeholder store install)
**Action**: modify

Add `StoreBackend::new(FsMapChangeSetStore { map_dir })` alongside the other per-map store
backends so both overworld and homebase entities carry it. Load the persisted `MapChangeSet`
into an in-memory component/resource on setup (or load lazily on first publish).

#### 3. Populate from edits each save cycle
**File**: `crates/server/src/map/mod.rs`
**Action**: modify

In `save_dirty_chunks_debounced`, after `let content_dirty = instance.content_dirty_chunks.drain()...`
(mod.rs:619), merge into the durable change-set instead of discarding:

```rust
// Accumulate genuine edits into the durable change-set (the publish candidate set).
if !content_dirty.is_empty() || entities_dirty {
    let mut change_set = change_set_store.0.load(&())?.unwrap_or_default();
    change_set.chunk_candidates.extend(content_dirty.iter().copied());
    change_set.map_entities_changed |= entities_dirty;
    // meta_changed set when spawn points / seed change; default false otherwise.
    change_set_store.0.save(&(), &change_set)?;
}
```

Add `&StoreBackend<(), MapChangeSet, FsMapChangeSetStore>` to the system's per-map query tuple.
Keep the existing `content_dirty` use as the overworld publish filter for now (rewired in
Phase 7).

### Verification
#### Automated
- [x] pgrep guard passes
- [x] `cargo test -p server world_persistence` — add tests: change-set persists + reloads; edits across two save cycles accumulate; empty cycle leaves it unchanged
- [x] `cargo check-all`

#### Manual
- [x] none

---

## Phase 5: Homebase publish via change-set + equals-generated + chained delta

### Changes

#### 1. Replace snapshot read-back with change-set-sourced classification
**File**: `crates/server/src/map/homebase_publication.rs`
**Action**: modify (replace `read_authoritative_homebase_publish` internals)

Instead of `list_saved_chunk_positions(...)` over the whole disk dir (homebase_publication.rs:293,319),
iterate the durable change-set `chunk_candidates`. For each candidate, using the live
`VoxelMapInstance` + `VoxelGenerator` for the homebase map entity:

- `chunk_matches_generated(pos, generator)` → classify `PayloadSlotState::Tombstoned`
  (and record in `scope.tombstoned_chunks`); also `FsChunkStore::delete(pos)` so local load
  regenerates it.
- otherwise read the chunk's `ChunkFileEnvelope` and classify `PayloadSlotState::Present`
  (record in `scope.edited_chunks`).

Because the classification needs ECS access (generator + instance), do it in the request system
`handle_homebase_attestation_requests` (homebase_publication.rs:417) and pass the resolved slots
into `begin_homebase_publication`. Meta/map-entities slots come from `change_set.meta_changed` /
`map_entities_changed` (Present when changed, else `Absent`/omitted).

`server_revision` / `previous_manifest_hash` continue to come from `FsAcceptedMapHeadStore`
(homebase_publication.rs:363-375) → this makes the manifest a **chained delta**
(`previous_hash = accepted_head`).

#### 2. Build the manifest through the shared path
**File**: `crates/server/src/map/homebase_publication.rs`
**Action**: modify (replace `upload_and_build_unsigned_manifest`)

Replace the bespoke `present_descriptor` loop (homebase_publication.rs:596-609) with
`upload_publish_slot` calls per resolved slot (Present uploads the blob; Tombstoned does not),
then `finalize_manifest(payloads, map_id, owner, revision, previous_hash, Some(attestation))`.
Delete `present_descriptor` and the publish use of `list_saved_chunk_positions`/`parse_chunk_pos`
(keep `parse_chunk_pos` only if still referenced elsewhere).

The attestation is still signed over `descriptor_root` + `payload_scope` via
`verify_homebase_publication_attestation_request` (homebase_publication.rs:538) — now the scope
carries `edited_chunks` + `tombstoned_chunks`.

### Verification
#### Automated
- [x] pgrep guard passes
- [x] `cargo test -p server` (homebase_publication unit tests) — update `terrain_chunks`→`edited_chunks` (homebase_publication.rs:769); add: candidate equal-to-generated → Tombstoned slot + file deleted; candidate differing → Present; descriptor_root covers tombstones
- [x] `cargo check-all`

#### Manual
- [ ] Run live publish (server+client env from handoff.md), edit a few homebase chunks, F7 → server logs show only the edited chunks uploaded (a handful, not 343); manifest assembles + client signs/publishes

---

## Phase 6: Publish-acceptance confirmation (advance head + clear change-set)

### Changes

#### 1. Track in-flight published keys
**File**: `crates/server/src/map/homebase_publication.rs`
**Action**: modify

When granting, snapshot the published candidate keys + the new `MapRevision` keyed by
`manifest_hash` in a resource (mirror `PendingPublishBySaveId`, remote_publish.rs:124):

```rust
#[derive(Resource, Default)]
pub struct InFlightHomebasePublishes(pub HashMap<ManifestHash, InFlightHomebasePublish>);
pub struct InFlightHomebasePublish { pub map_id: MapInstanceId, pub revision: MapRevision, pub published_chunks: HashSet<IVec3> }
```

Include `manifest_hash` in `HomebaseAttestationResponse::Granted` (Phase 3).

#### 2. Client confirms after publishing
**File**: `crates/client/src/map_publication.rs`
**Action**: modify

After the client signs + publishes the manifest event to relays
(`NostrManifestPublishStore`), send `HomebasePublished { manifest_hash }` on `MapChannel`.

#### 3. Server handler advances head + clears change-set
**File**: `crates/server/src/map/homebase_publication.rs`
**Action**: modify (new system)

```rust
/// On client confirmation that the granted homebase manifest was published, advance the
/// accepted head and remove exactly the published keys from the durable change-set.
pub fn handle_homebase_published(
    mut receivers: Query<(Entity, &mut MessageReceiver<HomebasePublished>)>,
    player_identities: Query<&PlayerIdentity>,
    mut in_flight: ResMut<InFlightHomebasePublishes>,
    accepted_head_stores: Query<&StoreBackend<(), MapRevision, FsAcceptedMapHeadStore>>,
    change_set_stores: Query<&StoreBackend<(), MapChangeSet, FsMapChangeSetStore>>,
    registry: Res<MapRegistry>,
) {
    // resolve owner -> homebase map entity; pop the in-flight entry by manifest_hash;
    // accepted_head_store.save(&(), &revision); load change_set, remove published_chunks
    // (set-difference, preserving keys edited after the build snapshot), clear meta/entities
    // flags if they were published; save change_set.
    // Unknown hash -> warn! and ignore (stale/duplicate confirmation).
}
```

Register the system after `poll_homebase_attestation_uploads`.

### Verification
#### Automated
- [ ] pgrep guard passes
- [ ] `cargo test -p server` — add: confirmation advances accepted_head to the granted revision; published keys removed from change-set; keys edited after snapshot survive; unknown hash is ignored
- [ ] `cargo check-all`

#### Manual
- [ ] Live: edit homebase, F7, confirm server advances accepted_head; edit again, F7 → second manifest `previous_hash` chains to the first and publishes only the newly-edited chunk
- [ ] Round-trip: `mv worlds/ worlds.bak`, restart, revisit homebase → edits restore from Nostr; reverted/deleted chunk regenerates from seed (absent in restored save)

---

## Phase 7: Overworld sources from the durable change-set

### Changes

#### 1. Source overworld draft from the change-set
**File**: `crates/server/src/map/mod.rs`
**Action**: modify

In `save_dirty_chunks_debounced`, build the overworld `ServerMapPublishDraft.chunks`
(mod.rs:659-668) from the durable change-set's `chunk_candidates` rather than the per-cycle
`content_dirty` filter. For each candidate, classify via `chunk_matches_generated(pos, generator)`:
equal → `PayloadSlotState::Tombstoned` (+ `FsChunkStore::delete`); differ → `Present(envelope)`.
This gives overworld the same delete-via-tombstone capability and a single edit source. The
draft is journaled as today; `apply_publish_results` (remote_publish.rs:420) already advances
heads on publish success — extend it to also remove the published keys from the change-set
(reuse the Phase 6 clearing helper).

Add `&VoxelGenerator` and the change-set store backend to the query tuple. Keep the
`SERVER_MAP_REMOTE_PUBLISH` gate.

### Verification
#### Automated
- [ ] pgrep guard passes
- [ ] `cargo test -p server world_persistence map_transition` — existing overworld publish/restore tests pass; add: reverted overworld chunk publishes a Tombstone and regenerates on restore
- [ ] `cargo test-all`
- [ ] `cargo check-all`

#### Manual
- [ ] Live overworld: edit chunks (server publishes incrementally as before); revert a chunk to generated → next publish tombstones it; round-trip restore regenerates it from seed

---

## Post-implementation

- [ ] Update `README.md` if publish behavior/commands are documented (F7 flow, deletion semantics)
- [ ] Strip all `// DEBUG` lines (`grep -rn "// DEBUG" crates/server/src crates/client/src`) before final commits
- [ ] `cargo test-all` green
