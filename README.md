# Untitled Brawler

Untitled Brawler is a multiplayer 2.5D brawler prototype built with Rust, Bevy, Lightyear, and a custom voxel map engine.

See [VISION.md](VISION.md) for the long-term game vision.

## Current Features

- Server-authoritative multiplayer over WebTransport with Lightyear prediction/replication.
- Native and WASM clients that connect to the same authoritative server.
- Nostr identity, server discovery, and connection authentication.
- Persistent voxel maps with Overworld and per-player Homebase instances, gated by server-side persistence preflight before startup spawns or map transitions.
- Server-authoritative terrain sculpting, world-object placement, editing, and persistence.
- Data-driven abilities, combat phases, hit detection, health, death, and respawn.
- Sprite-rig character rendering and RON-loaded animation assets.
- In-game dev inspector for physics debug, world inspection, spawn tools, and terrain tools.

## Prerequisites

- Rust toolchain from `rust-toolchain.toml`
- `openssl` for local WebTransport certificates
- `cargo-make` for aliases that call `cargo make`
- `wasm-pack` and Firefox for WASM tests

Initial setup:

```bash
sh scripts/setup.sh
```

This installs the WASM target, installs Bevy CLI, and generates local certificates.

## Running

Start the server:

```bash
SERVER_NSEC='nsec1...' cargo server
```

If the server identity is encrypted, also set:

```bash
NOSTR_IDENTITY_PASSPHRASE='...' SERVER_NSEC='ncryptsec1...' cargo server
```

Start the native client:

```bash
cargo client
```

Start the web client:

```bash
cargo web
```

The server listens on WebTransport at `0.0.0.0:5001`. Native and web clients default to `127.0.0.1:5001`.

Useful variants:

```bash
cargo server-log      # run server and tee clean logs to server.log
cargo client-log      # run client and tee clean logs to client.log
cargo server-tracy    # run server with tracy feature
cargo client-tracy    # run client with tracy feature
```

## Nostr Identities

Do not commit or log `nsec1...` private keys.

- Server identity comes from `SERVER_NSEC` first, otherwise from an encrypted profile identity.
- Encrypted `ncryptsec1...` or profile identities require `NOSTR_IDENTITY_PASSPHRASE`.
- Client identity is created or imported from the login screen, then stored encrypted under the Nostr config directory.
- Use profiles when running multiple identities locally:

```bash
cargo server -- --nostr-identity dev-server
cargo client -- --nostr-identity alice
```

Server discovery uses Nostr kind `30078` with identifier `untitled-brawler`. Override relays with:

```bash
NOSTR_RELAYS='wss://relay.damus.io,wss://nos.lol' cargo client
```

## Development Commands

```bash
cargo check-all       # cargo check --workspace
cargo build-all       # build native workspace crates, excluding web
cargo web-build       # release build for wasm32-unknown-unknown
cargo test-native     # native tests, excluding web
cargo test-wasm       # wasm-pack Firefox tests for crates/web
cargo test-all        # native then WASM tests
```

Regenerate local certificates:

```bash
sh certificates/generate.sh
# or
cargo make generate-certs
```

## Controls

- `WASD` / left gamepad stick: move
- `Space` / gamepad south: jump
- `Q` / `E`: rotate camera
- `1`-`4`: ability slots
- Left/right mouse: place/remove terrain or interact with armed edit tools
- `Delete`: delete selected world object
- `F3`: physics debug
- `F4`: dev inspector root
- `F5`: world inspector
- `F6`: spawn panel

Gameplay hotkeys are filtered through client-local input ownership so focused UI/text input does not trigger gameplay actions.

## Assets and Game Data

- Abilities: `assets/abilities.manifest.ron` and `assets/abilities/*.ability.ron`
- Default ability loadout: `assets/default.ability_slots.ron`
- Terrain definitions: `assets/terrain.manifest.ron` and `assets/terrain/*.terrain.ron`
- World objects: `assets/objects.manifest.ron` and `assets/objects/*.object.ron`
- Voxel models: `assets/models.manifest.ron` and `assets/models/**`
- Sprite rigs/animations: `assets/rigs/**` and `assets/anims/**`

## Project Structure

| Path | Purpose |
| --- | --- |
| `crates/protocol` | Shared replicated gameplay types, messages, abilities, terrain/object assets, auth, and transitions. |
| `crates/server` | Authoritative server gameplay, map lifecycle, persistence, world objects, and Nostr announcements. |
| `crates/client` | Native client app, input ownership, map rendering/editing integration, auth, and identity persistence. |
| `crates/web` | WASM client entrypoint. |
| `crates/client_lightyear` | Native WebTransport client networking setup. |
| `crates/client_web_lightyear` | WASM WebTransport client networking setup. |
| `crates/server_lightyear` | WebTransport server networking setup. |
| `crates/render` | Camera, lighting, health bars, sprite-rig rendering, and visual helpers. |
| `crates/ui` | Login, server list, menus, HUD, and map-switch UI. |
| `crates/dev` | Dev inspector, physics debug, spawn/world/terrain panels. |
| `crates/voxel_map_engine` | Voxel terrain generation, chunk lifecycle, meshing, brush edits, and map internals. |
| `crates/sprite_rig` | Sprite rig assets, animation loading, and billboarded rig spawning. |
| `crates/nostr_client` | Nostr relay pool, encrypted identity, server announcements, auth helpers, generic event queries, and verified blob helpers. |
| `crates/nostr_map_persistence` | Shared Nostr map persistence DTOs, signed manifest verification, query policies, remote read/write helpers, and store adapters. |
| `assets` | Game data, sprites, rigs, animations, terrain, objects, and voxel models. |
| `worlds` | Local generated/persisted world data. |
| `docs` | Brainstorms, task plans, bug notes, and deeper design documents. |
| `git` | Checked-out dependency sources and submodules. |
| `certificates` | Local WebTransport certificates and digest. |
| `scripts` | Setup and helper scripts. |

## Nostr/Blossom Map Persistence

Persistent maps still write to local filesystem stores first. When remote publishing is enabled, server-owned Overworld saves are tracked with per-map `remote_publish_journal.bin` entries, `local_head.bin`, and `accepted_head.bin`; failed remote publishes block later remote entries while preserving local files. Blossom uploads are authorized with BUD-11 `Authorization: Nostr ...` tokens signed by the configured Nostr keys. Remote restore is enabled by `SERVER_MAP_REMOTE_READ=1` or implicitly by `SERVER_MAP_REMOTE_PUBLISH=1`; startup preflight then queries Nostr for the latest server-owned Overworld manifest and downloads Blossom payloads from `SERVER_BLOSSOM_PUBLIC_BASE_URL`/`SERVER_BLOSSOM_ALLOWED_HOSTS` before falling back to local files.

Enable server-owned Overworld remote publishing with:

```bash
SERVER_MAP_REMOTE_PUBLISH=1 \
SERVER_BLOSSOM_UPLOAD_URL='https://blossom.example/upload' \
SERVER_BLOSSOM_PUBLIC_BASE_URL='https://blossom.example/' \
SERVER_NSEC='nsec1...' \
NOSTR_RELAYS='wss://relay.damus.io,wss://nos.lol' \
cargo server
```

For restore-only testing without publishing, use `SERVER_MAP_REMOTE_READ=1` with `SERVER_BLOSSOM_PUBLIC_BASE_URL` or `SERVER_BLOSSOM_ALLOWED_HOSTS`, `SERVER_NSEC`, and `NOSTR_RELAYS`. To test a clean remote restore safely, move `worlds/` aside instead of deleting it, then restore the backup if remote materialization fails.

For manual failure-path testing, add `SERVER_MAP_REMOTE_PUBLISH_FAIL_FIRST=1` to force the first manifest publish to fail after Blossom payload upload.

### Scope, Quarantine, and Recovery

v1 remote persistence covers map/layout data for the Overworld and Homebases only,
and never progression-bearing client-published state. "Latest" means the latest
*visible valid descendant* of the local accepted head under the configured relay
query policy — not a global latest: relays that are down or not configured cannot
contribute manifests.

Failure handling is split by class (each logged distinctly):

- **Unavailable** (relay query/Blossom fetch failed, timeout): graceful fallback to
  local filesystem state; nothing is written or blocked.
- **Invalid / Incomplete / Divergent** (bad signature/attestation, descriptor-root
  mismatch, blob hash/size mismatch, disallowed Blossom host, missing ancestor
  manifests, forked revision chain): the map is blocked from remote restore and a
  quarantine record is written; valid local filesystem state is never overwritten.

Quarantine records are RON files under `worlds/quarantine/<map>/` (override with
`SERVER_MAP_QUARANTINE_DIR`), named by manifest hash (or `local-invalid-<timestamp>`
when no hash applies), each recording the map id, owner, and rejection reason.

On-disk layout per map dir (`worlds/overworld/`, `worlds/homebase_<npub>/`):
`active_revision` (pointer file naming the active materialized revision),
`revisions/rev-<n>-<hash>/` (immutable materialized snapshots), `staging/`
(incomplete materialization work, cleaned at startup), and the top-level
`accepted_head.bin`/`local_head.bin`/`change_set.bin` head files. At startup the
server validates the active pointer; if it references a missing/incomplete
revision, it quarantines a record, removes the pointer, and rolls back to the
top-level filesystem state.

Manual recovery/rollback: stop the server, inspect `worlds/quarantine/` and the
`active_revision` pointer, unset `SERVER_MAP_REMOTE_READ`/`SERVER_MAP_REMOTE_PUBLISH`
to run filesystem-only (no migration needed — remote can be disabled at any time),
and delete `active_revision` to fall back to the legacy top-level save files (or
point it at a known-good `revisions/` directory).

### Player-Owned Homebase Publication

A player can publish their own Homebase to Nostr. Because the client cannot
faithfully reproduce the server's authoritative save bytes from replication, the
flow is "server encodes, client signs": the client presses `F7` (the
`PublishHomebase` action) to send a `HomebaseAttestationRequest`; the server
classifies the durable change-set of edited chunks against freshly-generated
terrain — chunks that still differ are uploaded to Blossom as `Present`, chunks
reverted to generated terrain become `Tombstoned` (and their local files are
deleted so they regenerate from seed). Chunks whose per-chunk world objects were
placed/removed/moved/rotated are published the same way (their current object
list, `Present`). The server chains the delta onto the accepted head,
signs a `HomebasePublicationAttestation`, and returns an unsigned
`NostrMapManifest`; the client signs that manifest event with the **player's**
Nostr key and publishes it to relays, then confirms back to the server, which
advances the accepted head and clears the published keys from its change-set so
the next publish chains onto this revision. Each publish is a small chained delta
of genuine edits, not a full snapshot. Remote import of a Homebase manifest is
accepted only if it carries a valid player signature and a valid server
attestation (the temporary Phase 3 insecure import path is removed).

Homebase publication requires the same server remote-publish configuration as the
Overworld (`SERVER_MAP_REMOTE_PUBLISH=1` with the `SERVER_BLOSSOM_*`, `SERVER_NSEC`,
and `NOSTR_RELAYS` settings above), plus a client running with a loaded Nostr
identity and configured relays.

> Not yet enforced: progression-bearing-data rejection and per-player entitlement
> checks on imported Homebase data (plan item 5.7) are deferred — no progression or
> entitlement types exist in the codebase yet. The server attestation requirement is
> the current Homebase import security boundary; progression-data filtering is a
> follow-up.
