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

1. The eat, dig, drop-collection and navigation state machines.
2. `Physics::tick`.
3. World re-centring, eviction and sub-chunk requests.
4. Encoding one `PlayerAuthInput` into the batch buffer.

Everything encoded during the tick is sent as a single batch. Inbound packets are decoded directly from the decompressed batch slice (`WireReader`), without an intermediate packet tree. `Bot` handles are cheap clones that send commands to the task and read state through a short-lived lock.

## Server-authoritative movement

BDS with `server-authoritative-movement` checks every `PlayerAuthInput` against its own simulation. When the two disagree, it sends `CorrectPlayerMovePrediction` with the authoritative position, velocity, on-ground flag and the input tick it corrected. On a `MovePlayer` teleport or reset, the server sends only the new position.

For both packets, when they target the bot, the driver does four things:

- **Snaps the simulated player.** It sets the position and replaces the velocity: the server's velocity for a correction, zero for a teleport. It also clears the fall distance and the jump cooldown. The velocity is clamped to 10 blocks/tick per axis and non-finite values are dropped, so a malformed packet cannot make the next collision sweep enumerate an unbounded region.
- **Never reports the jump as movement.** The delta on each `PlayerAuthInput` is measured from the position that tick started at, which after a correction is the corrected position, so the server never sees a jump from the stale one.
- **Acknowledges teleports.** It sets `HandledTeleport` on the next input.
- **Keeps navigation going.** The path follower resets its stall detection and re-anchors to the step nearest the corrected position — forward if the server pushed the bot along the path, backward if it pulled it back down the part already walked. If no step is within 2.5 blocks, it re-plans instead of giving up.

The input tick is *not* touched. The tick in both packets is the bot's own `PlayerAuthInput` tick echoed back, and the local counter doubles as the clock every pending task measures its timeout against, so it stays a monotonic local clock that advances by exactly one per tick. A correction naming a tick the bot has not sent yet cannot be answering one of its inputs and is ignored.

A rotation-only `MovePlayer` (mode 3) changes only yaw and pitch. `BotEvent::MovementCorrected` reports each applied correction, and `BotState::corrections` counts them.

## Modal forms

Server menus, warps, shops and rank selectors on Bedrock use JSON modal forms.

- **Receiving.** A `ModalFormRequest` produces `BotEvent::FormRequest { form_id, data }`, and the form stays in `Bot::open_forms()` until it is answered.
- **Answering.** Use one of these methods:
  - `click_form_button(id, index)` for simple (menu) forms.
  - `submit_form(id, json)` for raw responses: `"true"`/`"false"` for modal forms, or an array of element values for custom forms.
  - `close_form(id)` to close a form without answering.
- **Waiting.** `wait_for_form(timeout, filter)` waits for a form whose JSON matches.
- **Server close.** `ClientBoundCloseForm` clears all open forms and emits `BotEvent::FormsClosed`.
- **Limits.** At most 4 unanswered forms (up to 64 KB of JSON each) are kept per bot, in arrival order. When a fifth arrives, the oldest is cancelled with the `UserBusy` reason to make room. A request that reuses the id of a form already open replaces it instead, so it never costs another form its place.

## Eating

`bot.eat()` eats the best food in the inventory, and `bot.eat_item(name)` eats a specific one. With `BotConfig::auto_eat` (on by default), the bot eats by itself.

- **When it eats automatically.** When `food <= 14` or `health < 18`, controlled by `BotConfig::eat`. It only starts while on the ground and out of liquid, and not in the middle of a route unless health or food is critically low.
- **The sequence**, which follows the vanilla client:
  1. The food is moved into the selected hotbar slot if needed.
  2. One click-air `UseItem` transaction is sent together with the `StartUsingItem` input flag.
  3. The item is held in use for 32 ticks.
  4. A second click-air `UseItem` is sent, followed by a consume `ReleaseItem`.
- **Why that sequence.** BDS, PocketMine and Dragonfly all consume food on the second click-air, and none of them double-consumes on the release.
- **Confirmation.** Eating counts as successful when the hunger attribute rises or when the server's eating `ActorEvent` (`Feed`) arrives for that food.
- **Afterwards.** The previous hotbar slot is restored and `BotEvent::Ate` is emitted.
- **Which foods.** Only items in `AutoEatConfig::foods` are eaten, best first. The default list holds just the foods that are always safe: rotten flesh, raw chicken, spider eyes, pufferfish, poisonous potatoes and suspicious stew (whose effect is unknown until eaten) are never eaten unless you add them, though `eat_item` will still eat them on request. With a full hunger bar, normal food is refused straight away (golden apples and other always-edible items are still allowed).

## Dropped items

`AddItemActor` spawns are tracked in the entity table with their item id and count. They are removed on `TakeItemActor` (picked up by anyone) or `RemoveActor` (despawned).

- `Bot::nearby_drops(r)` lists the dropped items within `r` blocks.
- `Bot::collect_drops(r)` walks onto each of them, nearest first. On each item it waits up to about 2 s for the server's pickup, and it skips items it cannot reach or pick up within about 5 s. It returns how many the server handed over, counting a `TakeItemActor` naming this bot as the taker; an item that despawns or is taken by someone else is not counted.
- `Bot::dig_with(pos, DigOptions { collect_drops: true })` does the same for whatever drops within 4 blocks of a block the bot has just broken.
- Collection walks the bot with the same route slot as `goto`, so the two do not run at once: starting a route ends an active collect run (reporting what it had collected), and starting a collect run cancels the route. `stop()` ends both. A run is also ended by death or a dimension change, since the items it was walking to no longer exist. Whichever side loses is always answered — no call is left waiting.

## Memory budget

The per-bot target is under 1 MiB. It is held to that by:

- **World window.** The window is `(2r+1)²` columns, with `r = 2` by default.
  - Sub-chunks within `hot_radius` (default 1) of the bot's Y keep full paletted data. Decoded storage is re-packed to the smallest bit width.
  - Sub-chunks further away become 512-byte collision masks.
  - Uniform-air sub-chunks cost nothing.
  - Columns that leave the window are dropped immediately.
- **Entities.** At most 48 entities are tracked, in a contiguous `Vec`. Anything further than 48 blocks is evicted, except players. Entity type strings are interned.
- **Forms.** Open forms are bounded as described above.
- **Shared data.** The block registry, item registry and recipe book are de-duplicated process-wide, so bots on the same server share one copy. The vanilla block registry for one protocol and id mode is about 200 KB with sequential ids and about 540 KB with hashed ids. It is built on first use (about 25 ms) and freed when the last bot using it is dropped.
- **Pathfinding.** Searches are capped at 4000 nodes and 25 ms, which is about 200 KB of transient memory, freed afterwards.

`tests/memory_budget.rs` checks this in two ways:

- **Worst case, by accounting.** It fills one bot's state completely: 25 columns × 24 sub-chunks of 40 random block states, 48 entities, a full inventory and four 16 KB forms. That comes to about 656 KB. `Bot::heap_bytes()` reports the live figure.
- **Real allocations.** It counts heap allocations with a global allocator while 16 bots run for 10 simulated seconds, each with its full window loaded. That comes to about 147 KB per bot, and it includes the driver task, channels and buffers. The shared registry is excluded because it exists once per process.

Neither measurement covers the engine transport: RakNet queues, encryption and compression state are not included, because the tests use an in-memory transport. Measure process RSS with many real connections before you rely on a per-bot figure.

## Block data

### How runtime ids map to blocks

Chunks, `UpdateBlock` and item stacks refer to block states by a *network runtime id*. That id is meaningless without the server's block palette, which is the full list of block states the server knows (for 1.21.130, 15,845 states across 1,321 block types). `StartGame`'s `block_network_ids_are_hashes` flag decides how ids are assigned, and the bot reads it on every connection:

- **Sequential ids** (the flag is false). The canonical palette is stably sorted by the FNV-1 64 hash of each block *name*, keeping the states of one block in canonical order. The runtime id is the index into that list.
- **Hashed ids** (the flag is true). The runtime id is the FNV-1a 32 hash of the state encoded as little-endian NBT: `{name: <string>, states: {<properties sorted by key>}}`. `minecraft:unknown` is special-cased as `-2`. For example, air is `-604749536` and stone is `-2144268767`.

`BlockRegistry` implements both modes. Each state resolves to its name, properties, collision shape, liquid depth, hardness, friction and light.

### Where the palette comes from

In order of precedence:

1. **`BotConfig::canonical_block_states`.** A `canonical_block_states.nbt` you supply: a concatenation of network-NBT compounds with `name`, `states` and `version`, like the one in [pmmp/BedrockData](https://github.com/pmmp/BedrockData). You only need this for servers whose palette differs from vanilla (see below).
2. **The embedded vanilla palette for the negotiated protocol.** Fourteen BedrockData releases are compiled into `torchflower-world`, covering 1.21.50 to 1.26.30 (protocols 766 to 1001): 1.21.50, 1.21.60, 1.21.70, 1.21.80, 1.21.90, 1.21.93, 1.21.100, 1.21.111, 1.21.120, 1.21.130, 1.26.0, 1.26.10, 1.26.20 and 1.26.30. Each block stores its property values and the order its states are enumerated in, which takes about 14 KB per version compressed instead of more than 2 MB of NBT; releases whose palette is byte-identical share one blob, so the fourteen releases ship as nine files (148 KB in total). For a given protocol the newest palette at or below it is chosen, so 898 (1.21.130) uses the palette recorded for 897 and 860 uses the 1.21.120 one. A protocol newer than the table falls back to the newest palette, an older one to the oldest. Nothing has to be downloaded or passed in.
3. **`BlockRegistry::fallback`.** Only used if both of the above fail to decode: air, water and lava in hashed mode, with everything else treated as an unknown solid cube.

Tests check the embedded 1.21.130 palette against BedrockData's NBT, state by state. The order and the hash of all 15,845 states match, as do the published BDS hash values for air and stone.

### Custom palettes (add-ons, forks, future versions)

Servers with behaviour-pack blocks, or a Bedrock version newer than the embedded data, have a different palette. Because runtime ids are indices or hashes, one missing or extra state shifts or misses ids. To support such a server:

- **Newer vanilla version.** Download `canonical_block_states.nbt` from the matching BedrockData tag and pass it:

  ```rust
  let states = std::fs::read("canonical_block_states.nbt")?;
  config.canonical_block_states = Some(states.into());
  ```

- **Add-on blocks.** Custom blocks are sent in `StartGame`'s block-properties list and are not yet merged into the registry. With hashed ids (the default on modern BDS), custom blocks simply resolve as unknown solid cubes and vanilla blocks still resolve correctly. With sequential ids, custom blocks change the sort order, so supply a palette that already includes them.
- **Regenerating the embedded data.** After a new release, add its tag to `crates/torchflower-world/tools/gen_palettes.py` and run it; it rewrites `data/palettes/` and `src/palette_index.rs`.

### Block properties and shapes

- Hardness, friction and light come from BedrockData's `block_properties_table.json` (CC0). They are compiled in and can be regenerated with `crates/torchflower-world/tools/gen_block_table.py`.
- Collision shapes (slabs, stairs, fences, doors, trapdoors, carpets, snow layers) and preferred tools are derived from block names and states.

## Protocol notes

Packet layouts follow gophertunnel's definitions for protocols 766 (1.21.50) to 1001 (1.26.30). I compared every packet and shared structure the bot uses across the 14 protocol revisions in that range (766, 776, 786, 800, 818, 827, 844, 859, 860, 898, 924, 944, 975, 1001). These are the layout changes that affect the packets the bot sends or reads; each is handled based on the negotiated protocol:

| Protocol | Change |
|---|---|
| 776 (1.21.60) | The item table moves out of `StartGame` into the `ItemRegistry` packet, and item entries gain a version and NBT. Below 776 that packet id carries `ItemComponent` instead, so it is not read as a registry. |
| 818 (1.21.90) | `StartGame` drops the movement type from its move settings and gains an owner id. |
| 844 (1.21.111) | `StartGame`'s experimental-gameplay flag is a plain bool instead of an optional (it becomes optional again at 975). |
| 827 (1.21.100) | `CorrectPlayerMovePrediction` always has a rotation (before, only vehicle predictions did). |
| 859 (1.21.120) | `Animate` has a data float. |
| 898 (1.21.130) | `Text` gains a category byte plus constant strings; `CommandRequest`'s origin becomes the string `"player"` with an always-present unique id and its version becomes a string; `Animate` action becomes a byte with an optional swing source; `Interact` position becomes optional. |
| 924 (1.26.0) | The constant strings after the `Text` category are dropped; `StartGame`'s four id strings move to the tail of the packet. |
| 944 (1.26.10) | Block positions become signed in `UpdateBlock`, `ContainerOpen`, `PlayerAction` and use-item transactions; use-item transactions gain a cooldown byte. |
| 975 (1.26.20) | `MobEquipment` and `InventorySlot` use the compact item encoding; `InventorySlot` container and storage fields become optional. |
| 1001 (1.26.30) | `InventoryContent` and inventory transactions use compact items; the transaction header gains presence flags; action, face and prediction fields change type; `SubChunkRequest` puts the offset count first and the base position last as `int32`s. |

Protocol 2168 (1.26.40) and later are not implemented; `SubChunk` responses there use `Optional` markers.

The engine's own hand-written encoders were left unchanged. I found these differences between them and the reference:

- **`PlayerAuthInput` (engine `session.rs`).**
  - The reference writes `MoveVector` as a Vec2; the engine writes a Vec3.
  - The reference sends the interaction model as a varuint; the engine sends it zig-zag encoded.
  - The block-action count is zig-zag encoded in the reference.
  - The reference input-flag list has `StartJumping` at bit 31, so the later flags in `torchflower_protocol::compat::PlayerAuthInputFlags` are off by one: for example `PerformBlockActions` is bit 35 and `ClientAckServerData` is bit 44.
- **`Text`.** The typed `TextPacket` codec in `torchflower-protocol` uses the pre-898 layout.
- **`ItemStackRequest`.** The request id is a zig-zag varint in the reference.

These may be related to the gameplay blockers recorded in `NEXT_STEPS.md`. None of this has been checked against a live server yet.

## Validation status

- **Tested in-process.** Unit tests plus a fake-server suite (`crates/torchflower-bot/tests/fake_server.rs`) cover:
  - the login/spawn sequence and the 20 Hz input loop
  - digging with tool selection and server confirmation, with and without drop collection
  - block placement transactions
  - chat handlers and end-to-end navigation
  - movement corrections during navigation, teleport acknowledgement, and corrections whose tick is behind, ahead of, or absurdly far beyond the bot's own (ignored without disturbing the local clock)
  - modal forms: button, custom and modal responses, closing, server close, replacing a form by id, and evicting the oldest when the queue is full
  - automatic and manual eating, including the full-hunger rule and slot restore
  - collecting dropped items, collection reached through the dig timeout path, and the hand-offs between collection, navigation, `stop()` and death
  - a session with no palette file, using the embedded vanilla palette
  - the same session on protocols 924, 944, 975 and 1001
  - the `StartGame` walk and the `CommandRequest` bytes on all 14 supported protocols, including that a payload read with the wrong version's layout is rejected rather than trusted
  - memory use, both by accounting and by counting real allocations
- **Not yet tested against a real server.** Nothing has been run against BDS or any other server yet. Follow the local BDS workflow in `CONTRIBUTING.md` before you rely on gameplay on real servers.
- **Limitations:**
  - Movement uses vanilla-style physics. Corrections are reconciled as described above, but a server with stricter checks will issue more of them.
  - Crafting uses recipe-book (`CraftRecipeAuto`) requests. Item tags are matched with name heuristics.
  - Enchantments and status effects that affect dig speed are not decoded from item NBT or effect packets.
  - Vehicle movement predictions are ignored.
