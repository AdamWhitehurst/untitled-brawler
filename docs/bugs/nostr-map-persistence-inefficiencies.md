# Nostr Map Persistence: Architectural Inefficiencies

Findings from a multi-agent review of this branch vs `master` (10 tracer agents seeded
from random changed files, each finding adversarially verified by an independent agent;
18 claims confirmed, 15 rejected). Items 1-3 have real latency/memory impact; items 4-6
are confirmed-correct but modest cleanups.

**Status: items 1-5 fixed and verified (2026-06-04); item 6 deliberately skipped** —
the change-set file has four disk touchpoints (debounced save, publish-result clear,
two homebase publication sites), so a coherent memory cache would have to become the
source of truth across all of them: too much regression risk in verified flows for a
~1/sec small-file hitch. Deviations applied in item 4: canonical JSON sort key kept
(wire format), homebase `debug_assert` cross-check kept, per-manifest
`verify_descriptor_root` defense-in-depth on the read path kept.

Common root cause: the verification primitives (`compute_manifest_hash`,
`verify_descriptor_root`, `verify_blob_bytes`) are self-contained leaf functions, so
every layer defensively re-derives values instead of consuming the proof-carrying types
that already exist (`VerifiedManifest`, `VerifiedBlob`).

---

## 1. Serial network round-trips on publish and restore (HIGH)

**Locations**

- `crates/server/src/map/remote_publish.rs:277-332` — `prepare_server_map_publish_entry`
  awaits one `upload_publish_slot` at a time: meta, then each chunk, then each
  chunk-entities, then map-entities. N changed chunks ≈ 2N+2 sequential HTTP PUTs.
- `crates/server/src/map/homebase_publication.rs:866-873` —
  `upload_and_build_unsigned_manifest` awaits `blob_store.save` per blob in a for-loop.
- `crates/nostr_map_persistence/src/read.rs:298-318` — `download_payloads` awaits
  `fetch_and_verify_blob` one Present slot at a time.

**Why it is safe to fix**: blobs are content-addressed and independent;
`finalize_manifest` (`manifest.rs:217`) sorts descriptors so completion order cannot
change the manifest hash; all three paths already run inside a spawned `IoTaskPool`
task. NOT parallelizable: `fetch_manifest_ancestors` (`read.rs:217`) — the
`previous_hash` chain-walk is a strict data dependency.

**Proposed fix**: split `upload_publish_slot` into a sync
`prepare_publish_slot` (encode + content-address) and a
`upload_prepared_slots(blob_store, slots)` that drives the Present-blob PUTs with a
bounded `futures` `buffered(8)` stream; share it between the overworld and homebase
publish paths. Same bounded-stream treatment for the blob GETs in `download_payloads`.

**Regression tests**

- Unit test with a recording mock `AsyncStore<BlobRef, Vec<u8>>` whose `save` yields
  once before completing and tracks max in-flight count: assert all Present blobs
  uploaded, descriptors returned for every slot (Present + tombstoned), and
  `max_in_flight >= 2` (uploads overlap instead of running serially).
- Unit test: a failing mock store propagates `MapPersistenceRejection::Unavailable`.
- Unit test: `prepare_publish_slot` produces `blob.sha256 == Sha256(bytes)`, correct
  size, and the content-addressed public GET URL.
- Existing `verify_revision_chain` / `validate_remote_map_save` tests in `read.rs` must
  stay green (descriptor ordering unaffected).

**Manual verification**

```bash
# Publish path (overworld): run with remote publishing against a Blossom endpoint
SERVER_MAP_REMOTE_PUBLISH=1 \
SERVER_BLOSSOM_UPLOAD_URL='https://blossom.example/upload' \
SERVER_BLOSSOM_PUBLIC_BASE_URL='https://blossom.example/' \
SERVER_NSEC='nsec1...' \
NOSTR_RELAYS='wss://relay.example' \
cargo server-log
```

- Edit several terrain chunks, wait for the debounced save, and confirm the logs show
  `queued remote publish journal entry` followed by
  `remote manifest publish succeeded; advanced map heads`.
- Homebase path: with a client (`cargo client-log`, loaded Nostr identity), press `F7`
  and confirm the publish round-trip completes (`HomebasePublished` confirmation).
- Restore path: stop the server, move `worlds/` aside, restart with
  `SERVER_MAP_REMOTE_READ=1` plus the Blossom/relay env above, and confirm the map
  materializes from remote (multi-chunk world should preflight noticeably faster).

---

## 2. Every payload blob SHA-256 hashed twice on restore (HIGH)

**Locations**

- `crates/nostr_client/src/blobs.rs:305` — `fetch_and_verify_blob` runs
  `verify_blob_bytes` (full SHA-256 + size check) on download.
- `crates/nostr_map_persistence/src/read.rs:342` — `validate_remote_map_save` re-runs
  `verify_blob_bytes` on the identical bytes.
- `crates/server/src/map/preflight.rs:266-272` — the only production caller chains both
  inside one task with no trust boundary between them, so multi-MB terrain blobs are
  hashed twice for no security benefit.

**Proposed fix**: make `RawMapPayloads.present_payloads` private with two constructors —
`from_verified` (crate-internal, used by `download_payloads` since
`fetch_and_verify_blob` already proved the hashes) and `from_unverified` (public,
hashes each pair; used by tests/external construction so verification stays reachable).
`validate_remote_map_save` then trusts the wrapped bytes and drops its re-hash.

**Regression tests**

- `RawMapPayloads::from_unverified` rejects tampered bytes (`Invalid`, hash mismatch) —
  guards that verification remains reachable for untrusted construction.
- `from_unverified` rejects bytes supplied for a non-Present descriptor.
- Existing `validate_remote_map_save` tests (`read.rs:694`, `read.rs:751`) updated to
  the constructor and still pass (assembly behavior unchanged).

**Manual verification**: restore path as in item 1 — remote materialization succeeds;
to verify rejection still works, corrupt one blob on the Blossom server (or point a
descriptor at the wrong hash) and confirm the map is quarantined with a
`blob verification` rejection rather than materialized.

---

## 3. Full chunk data cloned on the publish path (HIGH)

**Location**: `crates/server/src/map/remote_publish.rs:284,288,305,329` —
`prepare_server_map_publish_entry` owns `draft` but clones `draft.meta`,
`draft.chunks` (full `ChunkFileEnvelope` voxel data), `draft.chunk_entities`, and
`draft.map_entities` before moving elements into `upload_publish_slot`. After the
loops only `draft.local_revision_number` (a `Copy` u64) is read.

**Proposed fix**: destructure `ServerMapPublishDraft` into its fields and iterate by
value — removes four allocations including a duplicate of every chunk's bytes. Folds
naturally into the item-1 refactor.

**Regression tests**: new unit test in `remote_publish.rs` driving
`prepare_server_map_publish_entry` end-to-end with a mock blob store
(meta + chunk-entities + map-entities draft): journal entry carries all descriptors,
`local_revision` matches the draft, and `signed_event_json` round-trips through
`manifest_hash_from_signed_event_json` to `new_manifest_hash`. (No such test exists
today; the prepare path is only covered transitively.)

**Manual verification**: publish path as in item 1 — identical journal/head behavior.

---

## 4. Manifest hash / descriptor root recomputed 3-4x per operation (MEDIUM, cold path)

Verifiers confirmed all counts but downgraded impact: these run once per
publish/preflight on network-bound paths. Worth doing for clarity.

- `crates/nostr_map_persistence/src/publish.rs:61-67` — one signing recomputes the
  descriptor root 3x and the manifest hash 4x (via `manifest_event_tags`,
  `verify_manifest_event_tags`, `verify_manifest_event_with_hash`).
- `crates/nostr_map_persistence/src/manifest.rs:383,444` —
  `verify_manifest_event_with_hash` itself computes the hash twice; the clearest
  single fix is reusing the inner hash.
- `crates/nostr_map_persistence/src/read.rs:169,274` — `fetch_manifest_by_hash` /
  `latest_visible_manifest` discard `VerifiedManifest.manifest_hash`; downstream
  chain-walk/validate recompute it per manifest.
- `crates/server/src/map/homebase_publication.rs:887-888` — the `descriptor_root`
  threaded into `upload_and_build_unsigned_manifest` is consumed only by a
  `debug_assert` (dead in release) while the root is recomputed and the descriptor
  list serialize-sorted ~3x (JSON sort keys; `PayloadClass`/`PayloadKey` derive `Ord`,
  so a tuple key suffices).

**Fix shape**: compute once, thread `ManifestHash`/root through; keep the post-sign
verify (deliberate round-trip check) but feed it the precomputed hash. Caveats from
verification: `compute_descriptor_root` must keep sorting internally (callers at
`read.rs` / `homebase_publication.rs:769` pass unsorted network data) — add a
precomputed-root finalize variant instead of stripping the sort.

**Regression tests**: existing manifest/publish round-trip tests; add an equality test
that the threaded-hash path produces byte-identical event JSON and tags vs the current
output for a fixed manifest.

---

## 5. Triple SHA-256 + double clone per blob upload (MEDIUM, cold path)

- `crates/nostr_map_persistence/src/payloads.rs:120` hashes to build the `BlobRef`;
- `crates/nostr_map_persistence/src/stores.rs:32` re-hashes via `verify_blob_bytes`
  (and its `VerifiedBlob` result is discarded — plus a full `value.clone()`);
- `crates/nostr_client/src/blobs.rs:214` hashes a third time for the BUD-11 auth token.

The post-upload check at `stores.rs:42` already validates the returned sha256/size
against the key, making the pre-upload verify redundant. Fix: drop the pre-check
(removes one hash + one clone) and pass the precomputed sha256 into `upload_blob` via
a new entry point (the existing one is shared with paths where re-hashing is
legitimate).

**Regression tests**: store-level test that a key/bytes mismatch is still rejected
(now by the post-upload check); existing `blobs_*` tests stay green.

---

## 6. Blocking filesystem I/O on the main schedule (MEDIUM, ~1/sec bound)

**Location**: `crates/server/src/map/mod.rs:605,616,628,633` —
`save_dirty_chunks_debounced` calls synchronous `Store::load`/`save` (meta,
change-set x2, local-head) inline in a main-schedule system, while the same system
routes chunk/entity writes through async `PendingStoreOps`. Gated behind the 1s
debounce, so a bounded hitch rather than a per-frame stall.

**Fix shape**: cache last-persisted meta and change-set in components, or route these
single-file reads/writes through `PendingStoreOps` like the chunk writes.

**Regression tests**: existing `server/tests/voxel_persistence.rs` debounce coverage;
add an assertion that change-set persistence still happens exactly once per debounce
window.

**Manual verification**: edit terrain continuously and confirm no save-tick frame
hitches (`cargo server-tracy` if measurement is wanted).

---

## Verified non-issues (do not "fix")

- Decode→re-encode on restore is required: payload zstd level differs from the fs
  stores' encoding, so received bytes are not the on-disk bytes.
- Relay-publish re-verification (`stores.rs:77`) and signed-event re-parse are
  deliberate trust-boundary checks.
- `fetch_manifest_ancestors` is inherently serial (`previous_hash` dependency).
- Homebase placeholder spawn is not duplicated — both paths share
  `spawn_homebase_preflight_placeholder_with_stores`.

## Verification commands (all items)

- `cargo check-all` then `cargo test-all` (never in parallel with another build).
- Manual paths: publish / homebase-F7 / remote-restore flows as listed in item 1,
  using the env documented in README "Nostr/Blossom Map Persistence".
