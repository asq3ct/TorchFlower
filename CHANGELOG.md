# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0-alpha] - Unreleased

### Added

- `torchflower-bot`: Mineflayer-style async `Bot` API. It covers chat and commands, `on_chat` handlers, digging with best-tool selection and server confirmation, block placement with reach and line-of-sight checks, equip/drop/swap, crafting, containers, navigation, following, attacking and respawning. A 20 Hz `PlayerAuthInput` loop drives it.
- `torchflower-world`: sparse per-bot voxel window made of hot paletted sub-chunks and cold collision masks. It also adds `LevelChunk`/`SubChunk` decoding, `SubChunkRequest` encoding, a block registry for both sequential and hashed runtime ids, a block-property table from BedrockData (CC0), and raycasts.
- `torchflower-physics`: AABB collision, gravity/drag/friction, step-up, jumping, sprinting, sneaking, swimming, climbing, fall damage and input flag generation.
- `torchflower-inventory`: inventory and container model; `InventoryContent`, `InventorySlot`, `ItemStackResponse`, `ItemRegistry` and `CraftingData` decoders; `ItemStackRequest` builders; a crafting planner.
- `torchflower-pathfinder`: bounded A* with walk, diagonal, ascend, descend, parkour, swim, bridge, pillar and dig moves, plus a path follower.
- `torchflower-protocol-core::wire`: zero-copy slice reader, writers, packet framing and network/little-endian NBT.
- `BedrockProtocolAdapter::recv_raw` and `observe_start_game`.

- Microsoft device-code authentication with Xbox Live, standard XSTS, PlayFab XSTS, PlayFab login, Minecraft entitlement session initialization, legacy Bedrock authentication, and Bedrock JWT chain generation.
- SQLite-backed account, token, entitlement, server, bot, and diagnostic persistence.
- Authenticated Axum REST API with exact-origin CORS, loopback-only unauthenticated development mode, and redacted diagnostics defaults.
- Bedrock client session support for RakNet handshake, ACK/NACK handling, fragmentation, reassembly, NetworkSettings, ZLib compression, login, encryption, resource pack acknowledgement, client cache status, StartGame processing, spawn observation, keepalive, chat, movement, inventory observation, and disconnect handling.
- DonutSMP-compatible NetworkStackLatency response encoding.
- Real-server validation for login, spawn, remained-connected, keepalive, chat, movement, inventory transactions, block-breaking evidence, and guarded block placing.
- Public `torchflower_engine::core` API types and examples for login, connect, chat, movement, block pickup, block placing, multi-bot supervision, and authenticated local API usage.

### Security

- API key authentication is required for `/api/*` routes by default.
- Token encryption uses strong key validation and redacted auth diagnostics.
- Server validation by raw host is restricted by `TORCHFLOWER_ALLOWED_SERVER_HOSTS`.
