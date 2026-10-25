# Untitled Brawler

A 2.5D brawler with a shared voxel world. Players edit terrain and place objects on maps the server persists locally and publishes to Nostr and Blossom so other servers and clients can restore them.

## Maps

**Map Instance**: One playable map the server is running, identified as the Overworld or as a player's Homebase.
_Avoid_: world, level, map entity

**Chunk**: A fixed-size cube of voxels, addressed by its position in the map's chunk grid.
_Avoid_: block, region

**Chunk Envelope**: A Chunk's voxel data together with the format version and chunk size it was saved under, so a loader can reject data from a different configuration.

**Map Meta**: The facts that define a Map Instance apart from its terrain, namely seed, generation version and spawn points.
_Avoid_: map config, map header

## Persistence

**Save Directory**: The on-disk home of one Map Instance, with one fixed layout for its Map Meta, map entities, Chunks, chunk entities and publish bookkeeping.
_Avoid_: map dir, store, save path

**Save Key**: The name of one thing the Save Directory can hold, such as the Map Meta or the Chunk at a position.

**Save Queue**: The per-map pump that carries writes and deletes to the Save Directory off the main thread, in order per Save Key.
_Avoid_: store ops, pending saves

**Save Batch**: The set of writes from one save cycle that become durable together. A publish revision is durable only when its whole Save Batch has landed.

**Change Set**: The Chunks, chunk entities, map entities and Map Meta edited since the Accepted Head. It is the candidate set for the next publish and survives restarts.
_Avoid_: dirty set, delta

**Publish State**: The in-memory record of a Map Instance's publish bookkeeping during a session, holding the Change Set, both heads and drafts awaiting durability. Disk follows it, not the other way round.
_Avoid_: publish ledger

## Publishing

**Revision**: One published version of a Map Instance, numbered and content-addressed by its manifest hash.

**Accepted Head**: The latest Revision the remote has accepted for a Map Instance.

**Local Head**: The latest Revision this server has made durable locally, which may be ahead of the Accepted Head.

**Publish Draft**: The payloads for one unpublished Revision, self-contained so it can be published after a restart without rereading the Save Directory.
_Avoid_: delta, pending publish

**Publish Journal**: The per-map record of every manifest publish attempt and its outcome.
