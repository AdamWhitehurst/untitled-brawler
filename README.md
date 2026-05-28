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
| `crates/nostr_map_persistence` | Shared Nostr map persistence DTOs, signed manifest verification, query policies, remote read helpers, and store adapters. |
| `assets` | Game data, sprites, rigs, animations, terrain, objects, and voxel models. |
| `worlds` | Local generated/persisted world data. |
| `docs` | Brainstorms, task plans, bug notes, and deeper design documents. |
| `git` | Checked-out dependency sources and submodules. |
| `certificates` | Local WebTransport certificates and digest. |
| `scripts` | Setup and helper scripts. |
