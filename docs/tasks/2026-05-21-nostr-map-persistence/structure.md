# Structure Outline

## Approach

Keep Nostr/Blossom transport in `nostr_client`, but let server/client game code own ownership policy, validation, reconciliation, and map lifecycle. The boundary is: fetch and validate remote manifests/payloads into backend-agnostic map-save data, assemble a complete save chain, materialize accepted data into the existing filesystem save layout, then reuse the existing map/chunk load pipeline.

Latestness means "latest visible valid descendant from the configured relay/query policy and local accepted head." The system can prove authenticity, integrity, and descent from a known head; it cannot prove global latest without a trusted sequencer/checkpoint service.

V1 scope is Overworld and Homebase only. Instanced stages are excluded unless a later design explicitly scopes them in.

## Phase 1: Preflight Persistence Before Transitions

Add an explicit server map persistence state and refactor map switching so target map metadata is selected before player relocation/freezing or `MapTransitionStart`.

**Files**: `crates/server/src/map.rs`, `crates/server/src/transition.rs`, `crates/server/tests/map_transition.rs`, `crates/server/tests/world_persistence.rs`

**Key changes**:

- `MapLoadState::{CheckingPersistence, AwaitingMeta, AwaitingEntities, Blocked(MapPersistenceBlockReason), Ready}` — extend lifecycle beyond entity existence
- `MapPersistenceOutcome::{UseFilesystem(MapMeta), UseRemote(ValidatedMapSave), Missing, RemoteUnavailable, Invalid, Incomplete, Divergent}` — server-only decision surface
- `PendingMapSwitchPreflight { client_entity, player_entity, current_map_id, target_map_id, requested_at }` — retain map-switch intent while async preflight runs
- `MapPersistencePreflightTask { request, rx: async_channel::Receiver<MapPersistencePreflightResult> }` — Bevy-polled remote preflight handle
- `spawn_map_persistence_preflight(...)` — spawn Nostr/HTTP work on `IoTaskPool`, matching existing `nostr_client` relay/announcement async-channel pattern
- `poll_map_persistence_preflight(...)` — poll receivers each `Update`, advance `MapLoadState`, and only then commit transition relocation/freeze/send
- `ensure_map_exists(...) -> MapPreparation` — return ready params or pending/blocked state instead of assuming registered maps are usable
- `start_map_transition(...)` — split into preflight and commit so relocation/freeze/send only happens after params are ready

**Verify**: after confirming no cargo build/check/test is running, `cargo test -p server map_transition world_persistence`; tests cover pending preflight retention, no relocation/freeze/start before params exist, unavailable remote fallback, blocked invalid/divergent states, and explicit `trace!` early-outs while waiting.

---

## Phase 2: Fake Remote Restore Materialization

Introduce a test remote backend and end-to-end restore path that selects the latest visible valid descendant, assembles a complete save, writes it into the existing filesystem layout, then lets normal map loading continue.

**Files**: `crates/server/src/persistence/mod.rs`, `crates/server/src/persistence/fs_map_meta.rs`, `crates/server/src/map.rs`, `crates/server/tests/world_persistence.rs`, `crates/server/tests/voxel_persistence.rs`

**Key changes**:

- `ValidatedMapSave { meta: MapMeta, chunks: Vec<(IVec3, ChunkFileEnvelope)>, chunk_entities: Vec<(IVec3, Vec<WorldObjectSpawn>)>, map_entities: Option<Vec<SavedEntity>>, revision: MapRevision }` — complete accepted save bundle
- `MapRevision { revision: u64, previous_hash: Option<[u8; 32]>, manifest_hash: [u8; 32] }` — local accepted-head metadata
- `AcceptedMapHeadStore` — filesystem-backed accepted head per map, introduced before real remote reads
- `bootstrap_filesystem_revision(save_dir: &Path, map_id: &MapInstanceId) -> Result<MapRevision, PersistenceError>` — legacy save chain root for existing filesystem worlds
- `RemoteMapPersistence` — server-side seam returning missing/unavailable/found/rejected outcomes without Nostr/Blossom types
- `MapPersistenceTestRemote { manifests: HashMap<ManifestHash, TestManifest>, heads: HashMap<MapInstanceId, ManifestHash>, payloads: HashMap<Hash, Vec<u8>> }` — test-only fake remote for unavailable/newer/invalid/incomplete/divergent/ancestor cases
- `assemble_validated_map_save(base: SaveBase, chain: Vec<ValidatedMapDelta>) -> Result<ValidatedMapSave, MapPersistenceRejection>` — fetch ancestors by hash, replay from genesis/filesystem base/local accepted head, and prove all required payload slots are complete before materialization
- `PayloadSlotState::{Present(BlobRef), Empty, Absent, Tombstoned}` — explicit semantics for chunks/entities so restore can distinguish empty content, missing content, and deletion
- `materialize_validated_map_save(save_dir: &Path, save: &ValidatedMapSave) -> Result<(), PersistenceError>` — write each file as `.tmp`, validate tmp content, rename into place, update `accepted_head` last, and delete leftover `.tmp` files on startup

**Verify**: `cargo test -p server remote_restore world_persistence voxel_persistence`; tests cover missing local save restored from fake remote, delta chain assembly from base plus updates, incomplete slot rejection, tombstone replay, interrupted tmp cleanup, accepted-head update last, and invalid/divergent data not overwriting filesystem state.

---

## Phase 3: Real Nostr/Blossom Read Path

Replace the fake remote source with signed Nostr manifest lookup and Blossom blob download while preserving the Phase 2 validation/materialization boundary. Blossom read support is verified through pure hash/size validation and fake transport tests; v1 does not add real Blossom integration tests or local HTTP servers.

**Files**: `crates/nostr_client/src/lib.rs`, `crates/nostr_client/src/map_persistence.rs`, `crates/nostr_client/Cargo.toml`, `crates/server/src/persistence/mod.rs`, `crates/server/src/map.rs`, `crates/nostr_client/src/relay_pool.rs`

**Key changes**:

- `NostrMapManifest { map_id, owner, revision, previous_hash, payloads, schema_version, descriptor_root }` — signed manifest event content and root of trust
- `ManifestPayloadDescriptor { class: PayloadClass, key: PayloadKey, blob: BlobRef, schema_version: u32 }` — binds each blob to its semantic slot
- `BlobRef { sha256: [u8; 32], size: u64, content_type: String, urls: Vec<String> }` — Blossom payload reference
- `MapPersistencePolicy { max_blob_bytes, max_manifest_bytes, max_payloads, allowed_payload_classes, entity_allowlist, map_bounds, quota, allowed_blossom_hosts }` — explicit bounds/quota/allowlist/URL policy
- `NostrMapQueryPolicy { relays, timeout, limit, tie_break }` — manifest kind/tags/filter/tie-break rules and timeout classification
- `verify_manifest_event(event_json: &str, expected_owner: NostrPublicKey, expected_map_id: &MapInstanceId) -> Result<NostrMapManifest, ManifestVerificationError>` — verifies Nostr signature, signer/owner, map id, schema, kind, and tags before payload fetch
- `verify_descriptor_root(manifest: &NostrMapManifest) -> Result<(), ManifestVerificationError>` — computes a domain-separated Merkle/root hash over payload class, key, blob sha256, size, and schema version
- `verify_revision_chain(candidate: &NostrMapManifest, accepted_head: Option<MapRevision>) -> Result<RevisionDecision, MapPersistenceRejection>` — rejects rollback, missing ancestors, ambiguous forks, and divergent heads; revision number is an ordering hint, not proof by itself
- `fetch_manifest_ancestors(head: &NostrMapManifest, accepted_head: Option<MapRevision>) -> Result<Vec<NostrMapManifest>, RemotePersistenceError>` — fetch by previous hash until accepted head/genesis/base, subject to query policy
- `verify_blob_url(url: &Url, policy: &MapPersistencePolicy) -> Result<(), BlossomReadError>` — require HTTPS and allowlisted/verified Blossom hosts before fetch
- `verify_blob_bytes(expected_sha256: [u8; 32], expected_size: Option<u64>, bytes: Vec<u8>) -> Result<VerifiedBlob, BlossomReadError>` — pure content-address verifier
- `BlobTransport::get(url: &str, max_bytes: u64) -> Result<Vec<u8>, BlossomReadError>` — production HTTP seam with fake implementation in tests
- `fetch_and_verify_blob(transport: &impl BlobTransport, blob: &BlobRef, limits: BlobLimits) -> Result<VerifiedBlob, BlossomReadError>` — tries policy-approved URLs and accepts only matching bytes
- `RemoteMapPersistenceClient::latest_visible_manifest(owner: NostrPublicKey, map_id: &MapInstanceId, policy: NostrMapQueryPolicy) -> Result<Option<NostrMapManifest>, RemotePersistenceError>`
- `RemoteMapPersistenceClient::download_payloads(manifest_chain: &[NostrMapManifest]) -> Result<RawMapPayloads, RemotePersistenceError>`
- `validate_remote_map_save(manifest_chain: Vec<NostrMapManifest>, payloads: RawMapPayloads, policy: MapPersistencePolicy) -> Result<ValidatedMapSave, MapPersistenceRejection>`

**Verify**: `cargo test -p nostr_client map_persistence && cargo test -p server remote_restore`; tests use byte fixtures, signed fixture manifests, one-field-at-a-time tamper mutations, and fake `BlobTransport` only. No local HTTP server, external Blossom server, or real-network integration test is required. Checks must prove tamper resistance for manifest signature/pubkey/map id/kind/tags/revision/previous hash/descriptor slot/blob hash/blob size, URL policy rejection for non-HTTPS/non-allowlisted hosts, timeout/unavailable classification, latest-safety relative to the local accepted head, and complete chain assembly before materialization.

---

## Phase 4: Server-Owned Overworld Dual-Write

After existing filesystem saves succeed, publish server-owned overworld chunk/entity updates and manifests to Nostr/Blossom through a serialized per-map journal so remote chains cannot fork due to concurrent publish jobs.

**Files**: `crates/server/src/map.rs`, `crates/server/src/persistence/mod.rs`, `crates/nostr_client/src/map_persistence.rs`, `crates/server/tests/voxel_persistence.rs`, `crates/server/tests/world_object_edit.rs`

**Key changes**:

- `RemotePublishJournalEntry { map_id, local_revision, previous_remote_manifest_hash, new_manifest_hash, payloads, status, retry_count }` — persisted publish unit
- `RemotePublishStatus::{Pending, InFlight, Published, Failed}` — explicit journal state
- `RemoteMapPublishWorker` — one in-flight publish per map, publishing only the oldest pending entry
- `RemotePublisher` and `BlobPublisher` seams — fake relay/blob publisher implementations for deterministic write-side tests
- `publish_server_map_update(identity: &ServerIdentity, update: ServerMapUpdate) -> Result<MapRevision, RemotePersistenceError>`
- `ServerMapUpdate { meta, chunks, chunk_entities, map_entities, previous_revision }` — server-owned publish unit
- `remote_head` advances only after publish success; failed entries retry before later entries; later pending entries may be squashed but must reference the current remote head
- Overworld manifests are accepted only from configured server identities; player/client-signed overworld manifests are rejected

**Verify**: `cargo test -p server voxel_persistence world_object_edit remote_publish`; tests cover publish N fails while N+1 is queued, one in-flight publish per map, retrying the same deterministic manifest without fork, crash/resume of pending/in-flight entries, remote already has manifest hash, and local filesystem advancing freely while remote chain advances serially.

---

## Phase 5: Player-Owned Homebase Publication

Allow clients to publish portable homebase map/layout updates only when a server-signed attestation binds the player-signed manifest root to authoritative replicated state. This rejects weaker "player signed after ack" trust.

**Files**: `crates/client/src/map.rs`, `crates/client/src/transition.rs`, `crates/nostr_client/src/map_persistence.rs`, `crates/protocol/src/map/persistence.rs`, `crates/server/src/map.rs`, `crates/client/tests/map_transition.rs`

**Key changes**:

- `HomebasePublicationQueue` — client resource tracking authoritative replicated changes eligible for publication
- `HomebaseReplicaCompleteness { has_meta: bool, terrain_chunks: HashSet<IVec3>, chunk_entities: HashSet<IVec3>, has_map_entities: bool }` — publish gate
- `ClientHomebaseUpdate { owner: NostrPublicKey, map_id: MapInstanceId, payloads, previous_revision, attestation }` — player-signed publish unit with server checkpoint
- `HomebasePublicationAttestation { owner, map_id, server_revision, previous_manifest_hash, descriptor_root, payload_scope, expires_at, server_pubkey, server_signature }` — server-signed authoritative content-root checkpoint
- `request_homebase_publication_attestation(...)` — client asks server to attest descriptor root after assembling payload hashes from its replica
- `verify_homebase_publication_attestation(...)` — server verifies descriptor/root matches authoritative homebase state for that revision before signing
- `publish_homebase_update(identity: &ClientIdentity, update: ClientHomebaseUpdate) -> Result<MapRevision, RemotePersistenceError>`
- Import accepts only if player signature is valid, signer equals owner, map id is `Homebase { owner }`, server attestation signature is valid, attestation owner/map/revision/descriptor root matches manifest, revision descends from accepted head, and payloads pass hash/schema/bounds/quota/allowlist validation
- Server import policy rejects progression-bearing objects, earned inventory, character state, relationships, breeding state, rewards, furnishings/toys/eggs/rewards not backed by entitlement, and any client-published overworld data

**Verify**: `cargo test -p client homebase_publication && cargo test -p server remote_restore`; manually perform a homebase edit, wait for server ack/replication, request server attestation, publish from client, then confirm server later imports only when both player signature and server attestation validate.

---

## Phase 6: Quarantine, Rollback, and Diagnostics

Keep invalid/divergent remote data out of live saves, support remote feature disablement, and add diagnostics/admin surfaces. Legacy bootstrap and accepted-head storage are already required in Phase 2/3.

**Files**: `crates/server/src/persistence/mod.rs`, `crates/server/src/map.rs`, `crates/nostr_client/src/map_persistence.rs`, `README.md`, `docs/tasks/2026-05-21-nostr-map-persistence/structure.md`

**Key changes**:

- `QuarantinedMapSave { map_id: MapInstanceId, owner: NostrPublicKey, reason: MapPersistenceRejection, manifest_hash: [u8; 32] }` — record rejected remote branches
- `RemoteMapPersistenceConfig { enabled: bool, fallback_timeout: Duration, quarantine_dir: PathBuf }` — runtime feature/degradation policy
- Startup cleanup removes leftover materialization `.tmp` files and resumes remote publish journal entries safely
- Structured logs include map id, owner, selected backend, revision, manifest hash, remote head, local accepted head, query policy, and failure class
- README documents v1 scope, latestness limitation, remote disable/rollback behavior, quarantine location, and manual recovery path

**Verify**: `cargo check-all && cargo test-all`; manually confirm existing filesystem-only worlds load unchanged, remote can be disabled without migration, divergent remote data is quarantined without overwriting valid filesystem state, and diagnostics expose enough data to identify relay unavailability vs invalid data vs divergent chains.

**Implementation note (2026-06-03)**: implemented as planned with these re-scopes against the final codebase. `QuarantinedMapSave.owner` is `Option<NostrPublicKey>` (None for Overworld) and `manifest_hash` is `Option<ManifestHash>` (rejections rarely carry one); records are RON under `quarantine_dir` (default `worlds/quarantine`, env `SERVER_MAP_QUARANTINE_DIR`), partitioned with the same `map_save_dir` naming as live saves. Startup recovery is per-map-dir (`recover_map_save_dir_for_loading`) at overworld init and homebase placeholder spawn rather than a global directory scan: it cleans `staging/`, validates the `active_revision` pointer against the completeness marker, and on invalid pointers quarantines a record, removes the pointer, and rolls back to top-level filesystem state (publish-journal `InFlight`→`Pending` reset already landed with Phase 4 startup recovery). Blocked remote preflights (invalid/incomplete/divergent, or failed materialization) also write quarantine records. Diagnostics classification lives in the `RemotePersistenceError → MapPersistenceRejection` conversion (`nostr_map_persistence/src/read.rs`, no separate `diagnostics.rs`): relay query and Blossom HTTP transport failures map to `Unavailable` (filesystem fallback, previously misclassified as `Invalid` → block), while manifest verification, descriptor-root mismatch, blob hash/size mismatch, and Blossom URL policy rejections map to `Invalid` with distinct message prefixes; divergence and missing ancestors keep `Divergent`/`Incomplete`. Remote-disable, rollback, quarantine location, and manual recovery are documented in README "Scope, Quarantine, and Recovery".

## Testing Checkpoints

- After Phase 1, transitions are gated by explicit server persistence states and async preflight never relocates/freezes/sends `MapTransitionStart` before seed/dimensions are known.
- After Phase 2, a complete validated fake remote chain can restore a missing local save through the existing filesystem/chunk pipeline, with accepted-head storage and safe temp-file materialization.
- After Phase 3, real Nostr/Blossom read failures are classified as unavailable vs invalid/incomplete/divergent with correct fallback/block behavior; signed manifests, descriptor roots, content hashes, URL policy, and revision-chain checks provide tamper resistance and latest-safety relative to the locally accepted head.
- After Phase 4, overworld saves dual-write after filesystem success, and the remote chain advances serially from the last published remote head without fork.
- After Phase 5, homebase publication is player-owned but accepted only with a matching server-signed authoritative content-root attestation.
- After Phase 6, feature disablement, quarantine, journal recovery, tmp cleanup, and diagnostics are safe enough for regular development.
