# TODO

## Planning

- [ ] Persistence
  - [x] abstract persistence to separate systems
  - [x] Filesystem Backends
  - [ ] NosterBackend
    - [ ] nostr saving of maps using editable nostr event + blossom blob storage
    - [ ] nostr prefetching, caching of chunks into fs
      - NostrBackend queries nearby
- [x] NostrClient for clients and server
  - [ ] not requiring `--nostr-identity` flag
  - [ ] user sign-up, set user info on updatable nostr event
    - [ ] Display name
  - [x] manages keys
  - [x] manages nostr relays
  - [ ] read events
  - [x] post events
  - [x] update events
  - [ ] delete events
  - [ ] login + ui
  - [ ] new keys + ui
  - [x] server discovery + ui
  - [ ] map ownership tied to nostr identities. Overworld = server's, Homebase = client's
  - [ ] chat messaging, includes map id

- [ ] Singleplayer
  - [ ] Persistence

---

## Bugs

- [ ] `FsLocalUnpublishedPublishDraftStore` is pinned to the startup `map_dir` in `init_overworld_entity` and is not re-pointed after a remote restore (same bug class as the head-store revision-reset fixed in the publish-restore work). Unpublished publish drafts written after a restore can be read from a different directory on the next startup, so a pending draft can be lost across a restore. Fix by giving drafts a fixed map-level (top-level) location, like the heads.

---

## Debt

- Remove FakeRemoteMapRestores
- Consolidate test Generator impl's
- WorldDirtyState.is_dirty is global. A homebase edit opens the debounce gate, and the save system then evaluates every map, including the overworld

## Considering

- Out of scope (flagging, not fixing): the FsLocalUnpublishedPublishDraftStore is pinned the same way and could lose unpublished drafts across a restore — same bug class
- [ ] Guiding player character, ability characters follow
- process distance sprite rigs at half the rate for performance?
- Social system that hooks into dialogue system
- Expose, split bevy-lightyear-template:
  - readme: how to add assets: animations, world objects, etc.
  - readme: built-in claude qrspi skills
  - stand-alone modules
- dampen player stats by distance from spawn?
- update world object system on hot reload by first remove WorldObjectId's components using old loaded Def, load new Def, insert_if_new(...) ?
- client --autoconnect flag
- stream ron assets to web clients on request
- wave function collapse
- Extend voxel_map_engine to support inserting pre-authored chunks
- composable Character templates that are loadable Asset files. Character template asset files are composed of other ron asset files
- monocraft ui font
- Ability unlock system
- Levelling system
- Singleplayer
- Stats and Buffs
- Npc interaction authoring via asset file writing
- NPCs
- Inventory/Item system
- animation creation still
- animation editor ui
- world object editor ui
- map editor ui
- spinning polyhedral exploding VFZ for ground_pound
