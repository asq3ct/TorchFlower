# Bot engine (`torchflower-bot`)

`torchflower-bot` puts a Mineflayer-style API on top of four standalone crates:

| Crate | Responsibility |
|---|---|
| `torchflower-world` | Sparse voxel window, block registry, chunk decoding, raycasts |
| `torchflower-physics` | 20 Hz movement simulation and `PlayerAuthInput` flags |
| `torchflower-inventory` | Inventory and containers, item stack requests, recipes and crafting |
| `torchflower-pathfinder` | Bounded A* and path following |

```rust
use torchflower_bot::{Bot, BotConfig, GoalGetToBlock};

let bot = Bot::connect(BotConfig::offline("127.0.0.1", 19132, "Miner")).await?;
bot.on_chat(|bot, _sender, message| async move {
    if message == "!mine iron" {
        if let Some(target) = bot.find_block("minecraft:iron_ore", 32) {
            bot.navigate_to(GoalGetToBlock(target.pos)).await?;
            bot.dig(target.pos).await?;
            bot.chat("Iron mined!").await?;
        }
    }
    Ok(())
});
```

See `crates/torchflower-bot/examples/miner.rs` for a runnable version. Only run it against servers you own or have permission to test on.

## Runtime model

Each bot runs one Tokio task. That task owns the connection and runs a `select!` over three things:

- a 50 ms tick
- API commands
- inbound batches

On every tick the task runs, in order:

1. The dig, place and navigation state machines.
2. `Physics::tick`.
3. World re-centring, eviction and sub-chunk requests.
4. Encoding one `PlayerAuthInput` into the batch buffer.

Everything encoded during the tick is sent as a single batch. Inbound packets are decoded directly from the decompressed batch slice (`WireReader`), without an intermediate packet tree. `Bot` handles are cheap clones that send commands to the task and read state through a short-lived lock.

## Memory budget

The per-bot state target is under 1 MiB. It is held to that by:

- **World window.** The window is `(2r+1)²` columns, with `r = 2` by default.
  - Sub-chunks within `hot_radius` (default 1) of the bot's Y keep full paletted data. Decoded storage is re-packed to the smallest bit width.
  - Sub-chunks further away become 512-byte collision masks.
  - Uniform-air sub-chunks cost nothing.
  - Columns that leave the window are dropped immediately.
- **Entities.** At most 48 entities are tracked, in a contiguous `Vec`. Anything further than 48 blocks is evicted, except players. Entity type strings are interned.
- **Shared data.** The block registry, item registry and recipe book are de-duplicated process-wide by payload hash, so bots on the same server share one copy.
- **Pathfinding.** Searches are capped at 4000 nodes and 25 ms, which is about 200 KB of transient memory, freed afterwards.

`tests/memory_budget.rs` fills a full window: 25 columns × 24 sub-chunks with 40 random block states each, plus 48 entities. That comes to about 590 KB of heap. `Bot::heap_bytes()` reports the live figure.

This budget covers bot *state* only. RakNet queues, encryption and compression buffers, and the Tokio task stack belong to the engine transport and are not counted here. Measure process RSS with many bots before you rely on a per-bot figure.

## Block data

Network runtime ids only resolve to block names if the registry knows the server's block palette.

- **Full palette.** Pass `canonical_block_states.nbt` from [pmmp/BedrockData](https://github.com/pmmp/BedrockData) for your server version via `BotConfig::canonical_block_states`. Both sequential ids (sorted by FNV-1 64) and hashed ids (FNV-1a 32 of the state NBT) are supported. The mode is chosen from `StartGame`.
- **Without a palette.** Only air, water and lava are known, and only in hashed mode. Every other block is an unknown solid cube: physics and pathfinding still work conservatively, but `find_block` cannot match names.
- **Where block semantics come from.**
  - Hardness, friction and light come from BedrockData's `block_properties_table.json` (CC0). It is compiled in and can be regenerated with `crates/torchflower-world/tools/gen_block_table.py`.
  - Collision shapes (slabs, stairs, fences, doors, carpets, snow) and preferred tools are derived from block names and states.
- **Custom blocks** from `StartGame` block properties are not added to the registry.

## Protocol notes

Packet layouts follow gophertunnel's protocol-898 (1.21.130) definitions. They cover protocols 766–1001 for the packets used here. The 1.26.40+ changes (protocol ≥ 2168, which uses `Optional` markers in `SubChunk`) are not implemented.

While building this I found three places where the engine's existing hand-written encoders differ from that reference. The bot uses its own encoders instead, and the engine paths were left unchanged:

- **`PlayerAuthInput` (engine `session.rs`).**
  - The reference writes `MoveVector` as a Vec2; the engine writes a Vec3.
  - The reference sends the interaction model as a varuint; the engine sends it zig-zag encoded.
  - The block-action count is zig-zag encoded in the reference.
  - The reference input-flag list has `StartJumping` at bit 31. The later flags in `torchflower_protocol::compat::PlayerAuthInputFlags` are therefore off by one: for example `PerformBlockActions` is bit 35 and `ClientAckServerData` is bit 44.
- **`Text`.** Protocol 898 added a category byte and constant strings. The typed `TextPacket` codec in `torchflower-protocol` uses the older layout.
- **`ItemStackRequest`.** The request id is a zig-zag varint in the reference.

This may be related to the gameplay blockers recorded in `NEXT_STEPS.md`. None of it has been checked against a live server yet.

## Validation status

- **Tested in-process.** Unit tests plus a fake-server suite (`crates/torchflower-bot/tests/fake_server.rs`) cover:
  - the login/spawn sequence
  - the 20 Hz input loop
  - digging with tool selection and server confirmation
  - block placement transactions
  - chat handlers
  - end-to-end navigation
  - memory use
- **Not yet tested against a real server.** Nothing has been run against BDS or any other server yet. Follow the local BDS workflow in `CONTRIBUTING.md` before you rely on gameplay on real servers.
- **Limitations:**
  - Movement uses vanilla-style physics. Servers with strict movement validation may still issue corrections; `CorrectPlayerMovePrediction` and `MovePlayer` are applied when they arrive.
  - Crafting uses recipe-book (`CraftRecipeAuto`) requests. Item tags are matched with name heuristics.
  - Enchantments and status effects that affect dig speed are not decoded from item NBT or effect packets.
