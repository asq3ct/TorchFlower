//! End-to-end tests against an in-memory fake Bedrock server.

use std::sync::Arc;
use std::time::Duration;

use torchflower_bot::protocol::{id, text_type};
use torchflower_bot::transport::memory::{pair, MemoryServer, Sent};
use torchflower_bot::{BlockPos, Bot, BotConfig, BotEvent, GoalBlock};
use torchflower_protocol::Packet;
use torchflower_protocol_core::wire::{
    begin_packet, end_packet, iter_packets, put_f32, put_string, put_ublock_pos, put_var_i32,
    put_var_i64, put_var_u32, put_var_u64, put_vec3, WireReader,
};
use torchflower_world::palette::local_index;
use torchflower_world::SubChunk;

const PROTOCOL: i32 = 898;
const RUNTIME_ID: u64 = 7;

fn nbt_state(out: &mut Vec<u8>, name: &str) {
    out.push(10);
    put_var_u32(out, 0);
    out.push(8);
    put_string(out, "name");
    put_string(out, name);
    out.push(10);
    put_string(out, "states");
    out.push(0);
    out.push(3);
    put_string(out, "version");
    put_var_i32(out, 1);
    out.push(0);
}

fn canonical_states() -> Arc<[u8]> {
    let mut out = Vec::new();
    for n in [
        "minecraft:air",
        "minecraft:stone",
        "minecraft:iron_ore",
        "minecraft:dirt",
    ] {
        nbt_state(&mut out, n);
    }
    out.into()
}

fn packet(batch: &mut Vec<u8>, id: u32, body: impl FnOnce(&mut Vec<u8>)) {
    let mark = begin_packet(batch, id);
    body(batch);
    end_packet(batch, mark);
}

struct Server {
    inner: MemoryServer,
    sent: Vec<(u32, Vec<u8>)>,
    typed: Vec<Packet>,
    protocol: i32,
}

impl Server {
    fn send(&self, batch: Vec<u8>) {
        self.inner.to_bot.send(batch).unwrap();
    }

    /// Drains everything the bot sent so far.
    fn drain(&mut self) {
        while let Ok(s) = self.inner.from_bot.try_recv() {
            match s {
                Sent::Raw(b) => {
                    for p in iter_packets(&b) {
                        let p = p.unwrap();
                        self.sent.push((p.id, p.payload.to_vec()));
                    }
                }
                Sent::Typed(t) => self.typed.extend(t),
                Sent::Latency(_) => {}
            }
        }
    }

    fn count(&self, packet_id: u32) -> usize {
        self.sent.iter().filter(|p| p.0 == packet_id).count()
    }

    /// All `PlayerAuthInput` packets sent so far, decoded.
    fn auth_inputs(&self) -> Vec<AuthInputView> {
        self.sent
            .iter()
            .filter(|p| p.0 == id::PLAYER_AUTH_INPUT)
            .map(|p| AuthInputView::parse(&p.1))
            .collect()
    }

    /// Payloads of every packet with `packet_id`.
    fn payloads(&self, packet_id: u32) -> Vec<Vec<u8>> {
        self.sent
            .iter()
            .filter(|p| p.0 == packet_id)
            .map(|p| p.1.clone())
            .collect()
    }

    fn clear(&mut self) {
        self.sent.clear();
        self.typed.clear();
    }
}

/// The fields of a `PlayerAuthInput` the tests check.
#[derive(Debug, Clone)]
struct AuthInputView {
    position: [f32; 3],
    flags: u128,
    tick: u64,
    delta: [f32; 3],
}

impl AuthInputView {
    fn parse(payload: &[u8]) -> Self {
        let mut r = WireReader::new(payload);
        r.skip(8, "pitch/yaw").unwrap();
        let position = r.vec3().unwrap();
        r.skip(8 + 4, "move vector/head yaw").unwrap();
        let mut flags: u128 = 0;
        let mut shift = 0;
        loop {
            let b = r.u8().unwrap();
            flags |= ((b & 0x7f) as u128) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        r.var_u32().unwrap();
        r.var_u32().unwrap();
        r.var_u32().unwrap();
        r.skip(8, "interact rotation").unwrap();
        let tick = r.var_u64().unwrap();
        let delta = r.vec3().unwrap();
        Self {
            position,
            flags,
            tick,
            delta,
        }
    }
}

fn rid(bot: &Bot, name: &str) -> u32 {
    bot.with_state(|s| s.world.registry().runtime_ids_for_name(name)[0])
}

/// Flat stone floor at y = 64 across column (0, 0) with iron ore at (7, 65, 8).
fn level_chunk(bot: &Bot) -> Vec<u8> {
    let air = rid(bot, "air");
    let stone = rid(bot, "stone");
    let ore = rid(bot, "iron_ore");
    let mut sub = SubChunk::uniform(air);
    let layer = sub.layers[0].as_mut().unwrap();
    for x in 0..16 {
        for z in 0..16 {
            layer.set(local_index(x, 0, z), stone);
        }
    }
    layer.set(local_index(7, 1, 8), ore);
    let mut body = Vec::new();
    sub.encode(4, &mut body);
    let mut batch = Vec::new();
    packet(&mut batch, id::LEVEL_CHUNK, |p| {
        put_var_i32(p, 0);
        put_var_i32(p, 0);
        put_var_i32(p, 0);
        put_var_u32(p, 1);
        p.push(0);
        put_var_u32(p, body.len() as u32);
        p.extend_from_slice(&body);
    });
    batch
}

/// How the fake server session is set up.
struct Setup {
    protocol: i32,
    /// `true`: pass a small custom palette; `false`: rely on the embedded
    /// vanilla palette (sequential ids, as the truncated StartGame here does
    /// not announce hashed ids).
    custom_palette: bool,
    /// Extra `(name, network_id)` item registry entries.
    items: Vec<(&'static str, i16)>,
    /// `(slot, network_id, count)` inventory contents.
    inventory: Vec<(u32, i32, u16)>,
    configure: fn(&mut BotConfig),
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            protocol: PROTOCOL,
            custom_palette: true,
            items: Vec::new(),
            inventory: vec![(0, 3, 16), (3, 300, 1)],
            configure: |_| {},
        }
    }
}

fn put_item(
    p: &mut Vec<u8>,
    protocol: i32,
    network_id: i32,
    count: u16,
    stack_id: i32,
    block: i32,
) {
    let item = torchflower_inventory::ItemStack {
        network_id,
        count,
        stack_id,
        block_runtime_id: block,
        extra: vec![0u8; 10].into(),
        ..Default::default()
    };
    // InventoryContent switches to compact items at 1001.
    if protocol >= 1001 {
        item.write_compact(p);
    } else {
        item.write_instance(p);
    }
}

async fn spawned_bot() -> (Bot, Server) {
    spawned_bot_with(Setup::default()).await
}

async fn spawned_bot_with(setup: Setup) -> (Bot, Server) {
    let (transport, server) = pair(setup.protocol);
    let mut cfg = BotConfig::offline("fake", 19132, "TestBot");
    if setup.custom_palette {
        cfg.canonical_block_states = Some(canonical_states());
    }
    cfg.spawn_timeout = Duration::from_secs(5);
    (setup.configure)(&mut cfg);
    let bot = Bot::start(transport, cfg, "0".into());
    let server = Server {
        inner: server,
        sent: Vec::new(),
        typed: Vec::new(),
        protocol: setup.protocol,
    };

    let mut batch = Vec::new();
    packet(&mut batch, id::RESOURCE_PACKS_INFO, |_| {});
    packet(&mut batch, id::RESOURCE_PACK_STACK, |_| {});
    packet(&mut batch, id::START_GAME, |p| {
        put_var_i64(p, RUNTIME_ID as i64);
        put_var_u64(p, RUNTIME_ID);
        put_var_i32(p, 0);
        put_vec3(p, [8.5, 65.0 + 1.62, 8.5]);
        put_f32(p, 0.0);
        put_f32(p, 0.0);
        p.extend_from_slice(&[0u8; 10]);
        put_string(p, "");
        put_var_i32(p, 0);
    });
    let mut items = vec![("minecraft:stone_pickaxe", 300i16), ("minecraft:dirt", 3)];
    items.extend(setup.items.iter().copied());
    packet(&mut batch, id::ITEM_REGISTRY, |p| {
        put_var_u32(p, items.len() as u32);
        for (name, nid) in &items {
            put_string(p, name);
            p.extend_from_slice(&nid.to_le_bytes());
            p.push(0);
            put_var_i32(p, 1);
            p.extend_from_slice(&[10, 0, 0]);
        }
    });
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;

    server.send(level_chunk(&bot));
    let dirt_block = rid(&bot, "dirt") as i32;
    let mut batch = Vec::new();
    packet(&mut batch, id::INVENTORY_CONTENT, |p| {
        put_var_u32(p, 0);
        put_var_u32(p, 36);
        for slot in 0..36u32 {
            match setup.inventory.iter().find(|i| i.0 == slot) {
                Some(&(_, nid, count)) => {
                    let block = if nid == 3 { dirt_block } else { 0 };
                    put_item(p, setup.protocol, nid, count, 50 + slot as i32, block);
                }
                None if setup.protocol >= 1001 => {
                    torchflower_inventory::ItemStack::default().write_compact(p)
                }
                None => put_var_i32(p, 0),
            }
        }
    });
    packet(&mut batch, id::PLAY_STATUS, |p| {
        p.extend_from_slice(&3i32.to_be_bytes())
    });
    server.send(batch);
    bot.wait_for_spawn().await.expect("spawn");
    (bot, server)
}

#[tokio::test(start_paused = true)]
async fn login_sequence_and_tick_loop() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    server.drain();

    // Handshake responses went through the typed encoder.
    assert!(server
        .typed
        .iter()
        .any(|p| matches!(p, Packet::ResourcePackClientResponse(r) if r.response_status == 4)));
    assert!(server.typed.iter().any(
        |p| matches!(p, Packet::SetLocalPlayerAsInitialized(i) if i.runtime_entity_id == RUNTIME_ID)
    ));
    assert!(server
        .typed
        .iter()
        .any(|p| matches!(p, Packet::RequestChunkRadius(_))));

    // ~20 PlayerAuthInput packets per second, with increasing ticks.
    let n = server.count(id::PLAYER_AUTH_INPUT);
    assert!((17..=22).contains(&n), "auth inputs: {n}");
    let ticks: Vec<u64> = server
        .sent
        .iter()
        .filter(|p| p.0 == id::PLAYER_AUTH_INPUT)
        .map(|p| {
            let mut r = WireReader::new(&p.1);
            r.skip(4 + 4 + 12 + 8 + 4, "prefix").unwrap();
            // flags varint
            while r.u8().unwrap() & 0x80 != 0 {}
            r.var_u32().unwrap();
            r.var_u32().unwrap();
            r.var_u32().unwrap();
            r.skip(8, "interact rot").unwrap();
            r.var_u64().unwrap()
        })
        .collect();
    assert!(ticks.windows(2).all(|w| w[1] > w[0]));

    // Standing on the floor: physics keeps the bot at y = 65.
    let pos = bot.position();
    assert!((pos.y - 65.0).abs() < 1e-6, "y = {}", pos.y);
    assert!(bot.with_state(|s| s.player.on_ground));
    assert_eq!(bot.inventory().get(3).unwrap().network_id, 300);
    assert!(bot.heap_bytes() < 64 * 1024);
}

#[tokio::test(start_paused = true)]
async fn dig_selects_tool_and_waits_for_confirmation() {
    let (bot, mut server) = spawned_bot().await;
    // Let physics settle the bot onto the floor (digging mid-air is 5x slower).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(bot.with_state(|s| s.player.on_ground));
    let ore = bot
        .find_block("minecraft:iron_ore", 16)
        .expect("ore visible");
    assert_eq!(ore.pos, BlockPos::new(7, 65, 8));

    let digger = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.dig(BlockPos::new(7, 65, 8)).await })
    };
    // Stone pickaxe on iron ore (hardness 3): 3 / 4 * 30 = 22.5 -> 23 ticks.
    tokio::time::sleep(Duration::from_millis(50 * 30)).await;
    server.drain();
    assert!(server.count(id::MOB_EQUIPMENT) >= 1, "selected the pickaxe");
    assert_eq!(bot.inventory().selected_hotbar(), 3);

    let mut actions = Vec::new();
    for (pid, payload) in &server.sent {
        if *pid != id::PLAYER_AUTH_INPUT {
            continue;
        }
        let mut r = WireReader::new(payload);
        r.skip(4 + 4 + 12 + 8 + 4, "prefix").unwrap();
        let mut flags: u128 = 0;
        let mut shift = 0;
        loop {
            let b = r.u8().unwrap();
            flags |= ((b & 0x7f) as u128) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        if flags & torchflower_physics::input_flags::PERFORM_BLOCK_ACTIONS == 0 {
            continue;
        }
        r.var_u32().unwrap();
        r.var_u32().unwrap();
        r.var_u32().unwrap();
        r.skip(8, "interact").unwrap();
        r.var_u64().unwrap();
        r.skip(12, "delta").unwrap();
        let n = r.var_i32().unwrap();
        for _ in 0..n {
            let a = r.var_i32().unwrap();
            r.block_pos().unwrap();
            r.var_i32().unwrap();
            actions.push(a);
        }
    }
    assert_eq!(actions.first(), Some(&0), "StartBreak first: {actions:?}");
    assert!(
        actions.contains(&26),
        "PredictDestroyBlock sent: {actions:?}"
    );
    let cracks = actions.iter().filter(|a| **a == 18).count();
    assert!((20..=24).contains(&cracks), "crack ticks {cracks}");
    assert!(!digger.is_finished(), "waits for server confirmation");

    let air = rid(&bot, "air");
    let mut batch = Vec::new();
    packet(&mut batch, id::UPDATE_BLOCK, |p| {
        put_ublock_pos(p, [7, 65, 8]);
        put_var_u32(p, air);
        put_var_u32(p, 3);
        put_var_u32(p, 0);
    });
    server.send(batch);
    let res = tokio::time::timeout(Duration::from_secs(2), digger)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()));
    assert!(bot.find_block("minecraft:iron_ore", 16).is_none());
}

#[tokio::test(start_paused = true)]
async fn chat_handler_and_navigation() {
    let (bot, mut server) = spawned_bot().await;
    let mut events = bot.events();
    bot.on_chat(|bot, sender, message| async move {
        if message == "!come" {
            bot.chat(format!("coming, {sender}")).await?;
            bot.navigate_to(GoalBlock::new(12, 65, 12)).await?;
            bot.chat("arrived").await?;
        }
        Ok(())
    });
    let mut batch = Vec::new();
    packet(&mut batch, id::TEXT, |p| {
        p.push(0);
        p.push(1);
        for c in ["chat", "whisper", "announcement"] {
            put_string(p, c);
        }
        p.push(text_type::CHAT);
        put_string(p, "Steve");
        put_string(p, "!come");
        put_string(p, "");
        put_string(p, "");
        p.push(0);
    });
    server.send(batch);

    let arrived = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            server.drain();
            let texts: Vec<_> = server
                .sent
                .iter()
                .filter(|p| p.0 == id::TEXT)
                .filter_map(|p| torchflower_bot::protocol::TextMessage::decode(&p.1, PROTOCOL).ok())
                .map(|m| m.message)
                .collect();
            if texts.iter().any(|m| m == "arrived") {
                assert!(texts.iter().any(|m| m == "coming, Steve"));
                break;
            }
        }
    })
    .await;
    assert!(
        arrived.is_ok(),
        "bot did not arrive; at {:?}",
        bot.position()
    );
    assert_eq!(
        bot.with_state(|s| s.player.block_pos()),
        BlockPos::new(12, 65, 12)
    );
    while let Ok(ev) = events.try_recv() {
        assert!(!matches!(ev, BotEvent::HandlerError(_)), "{ev:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn place_block_sends_transaction_and_waits() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Out of reach is rejected locally.
    assert!(matches!(
        bot.place_block(BlockPos::new(15, 64, 15), 1).await,
        Err(torchflower_bot::BotError::OutOfReach(_))
    ));
    // Occupied target (the ore block) is rejected locally.
    assert_eq!(
        bot.place_block(BlockPos::new(7, 64, 8), 1).await,
        Err(torchflower_bot::BotError::Occupied)
    );

    let placer = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.place_block(BlockPos::new(9, 64, 8), 1).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.drain();
    let tx = server
        .sent
        .iter()
        .find(|p| p.0 == id::INVENTORY_TRANSACTION)
        .expect("use-item transaction");
    let mut r = WireReader::new(&tx.1);
    assert_eq!(r.var_i32().unwrap(), 0);
    assert_eq!(r.var_u32().unwrap(), 2); // use item
    assert_eq!(r.var_u32().unwrap(), 0); // no actions
    assert_eq!(r.var_u32().unwrap(), 0); // click block
    assert_eq!(r.var_u32().unwrap(), 1); // player input
    assert_eq!(r.ublock_pos().unwrap(), [9, 64, 8]);
    assert_eq!(r.var_i32().unwrap(), 1); // face up
    assert_eq!(r.var_i32().unwrap(), 0); // hotbar slot
    let held = torchflower_inventory::ItemStack::read_instance(&mut r).unwrap();
    assert_eq!((held.network_id, held.stack_id), (3, 50));
    let player = r.vec3().unwrap();
    assert!((player[1] - 66.62).abs() < 1e-3);
    let click = r.vec3().unwrap();
    assert_eq!(click, [0.5, 1.0, 0.5]);
    assert_eq!(r.var_u32().unwrap(), rid(&bot, "stone"));
    assert_eq!(r.var_u32().unwrap(), 1);
    assert!(!placer.is_finished());

    let dirt = rid(&bot, "dirt");
    let mut batch = Vec::new();
    packet(&mut batch, id::UPDATE_BLOCK, |p| {
        put_ublock_pos(p, [9, 65, 8]);
        put_var_u32(p, dirt);
        put_var_u32(p, 3);
        put_var_u32(p, 0);
    });
    server.send(batch);
    let res = tokio::time::timeout(Duration::from_secs(2), placer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()));
    assert_eq!(
        bot.block_at(BlockPos::new(9, 65, 8)).unwrap().name,
        "minecraft:dirt"
    );
}

// ---------------------------------------------------------------------------
// Task 1: server-authoritative movement reconciliation
// ---------------------------------------------------------------------------

fn correction(batch: &mut Vec<u8>, feet: [f32; 3], velocity: [f32; 3], tick: u64) {
    packet(batch, id::CORRECT_PLAYER_MOVE_PREDICTION, |p| {
        p.push(0); // player prediction
        put_vec3(p, [feet[0], feet[1] + 1.62, feet[2]]);
        put_vec3(p, velocity);
        torchflower_protocol_core::wire::put_vec2(p, [0.0, 0.0]);
        p.push(0); // no vehicle angular velocity
        p.push(1); // on ground
        put_var_u64(p, tick);
    });
}

#[tokio::test(start_paused = true)]
async fn movement_correction_snaps_state_and_keeps_navigating() {
    let (bot, mut server) = spawned_bot().await;
    let mut events = bot.events();
    let nav = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.navigate_to(GoalBlock::new(13, 65, 8)).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.drain();
    let before = bot.position();
    assert!(before.x > 8.8, "bot started walking: {before:?}");
    let last_tick = server.auth_inputs().last().unwrap().tick;

    // Rubber-band the bot back to the start. The tick in the packet is the
    // bot's own input tick echoed back by the server, so a correction is
    // acknowledged for a tick that has already been sent.
    let server_tick = last_tick;
    let mut batch = Vec::new();
    correction(&mut batch, [8.5, 65.0, 8.5], [0.0, 0.0, 0.0], server_tick);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let snapped = bot.position();
    assert!(
        (snapped.x - 8.5).abs() < 1e-4 && (snapped.y - 65.0).abs() < 1e-4,
        "{snapped:?}"
    );
    assert_eq!(bot.velocity(), torchflower_bot::Vec3::ZERO);
    assert_eq!(bot.with_state(|s| s.corrections), 1);

    tokio::time::sleep(Duration::from_millis(60)).await;
    server.drain();
    let inputs = server.auth_inputs();
    // The first input after the correction reports the corrected position, and
    // its delta is one tick of walking — never the jump back to it.
    let first = inputs
        .iter()
        .find(|i| i.tick > last_tick)
        .expect("input after correction");
    assert!(
        (first.position[0] - 8.5).abs() < 0.3,
        "{:?}",
        first.position
    );
    assert!((first.position[1] - 66.62).abs() < 1e-3);
    assert!(first.delta[1].abs() < 1e-6, "no stale vertical delta");
    let jumped_back = (before.x - 8.5).abs() as f32;
    assert!(jumped_back > 0.3, "the correction really moved the bot");
    let reported = first.delta[0].hypot(first.delta[2]);
    assert!(
        reported < 0.3 && reported < jumped_back,
        "delta is one tick of walking, not the {jumped_back} block jump: {reported}"
    );
    assert!(
        inputs.windows(2).all(|w| w[1].tick == w[0].tick + 1),
        "the input tick is a local clock: it only ever advances by one"
    );

    // Navigation was not aborted by the jump and still reaches the goal.
    let res = tokio::time::timeout(Duration::from_secs(20), nav)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()));
    assert_eq!(
        bot.with_state(|s| s.player.block_pos()),
        BlockPos::new(13, 65, 8)
    );
    let mut saw = false;
    while let Ok(ev) = events.try_recv() {
        if let BotEvent::MovementCorrected { position, tick } = ev {
            assert_eq!(tick, server_tick);
            assert!((position.y - 65.0).abs() < 1e-4);
            saw = true;
        }
    }
    assert!(saw, "MovementCorrected event emitted");
}

/// A correction naming a tick the bot has not reached yet cannot be an answer
/// to one of its inputs, so it must be ignored rather than teleport the bot
/// (and must never be used to move the local tick counter, which every
/// pending task measures its timeout against).
#[tokio::test(start_paused = true)]
async fn correction_for_a_future_tick_is_ignored() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.drain();
    let before = bot.position();
    let last_tick = server.auth_inputs().last().unwrap().tick;

    let mut batch = Vec::new();
    correction(&mut batch, [8.5, 40.0, 8.5], [0.0, 0.0, 0.0], u64::MAX);
    correction(
        &mut batch,
        [8.5, 41.0, 8.5],
        [0.0, 0.0, 0.0],
        last_tick + 500,
    );
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.drain();

    assert_eq!(bot.position(), before, "not teleported by a bogus tick");
    assert_eq!(bot.with_state(|s| s.corrections), 0);
    let inputs = server.auth_inputs();
    assert!(
        inputs.windows(2).all(|w| w[1].tick == w[0].tick + 1),
        "local tick unaffected by the server's tick field"
    );
    assert!(
        inputs.last().unwrap().tick < last_tick + 500,
        "tick did not jump: {}",
        inputs.last().unwrap().tick
    );
    // Tick-based deadlines are still measured against a sane clock, so a
    // normal request completes instead of instantly expiring.
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        bot.navigate_to(GoalBlock::new(10, 65, 8)),
    )
    .await
    .unwrap();
    assert_eq!(res, Ok(()));
}

/// A correction for an older tick (the server answering an input the bot has
/// since moved on from) is still authoritative and must be applied.
#[tokio::test(start_paused = true)]
async fn correction_for_a_past_tick_is_applied() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.drain();
    let last_tick = server.auth_inputs().last().unwrap().tick;
    assert!(last_tick > 2, "bot has been ticking: {last_tick}");

    let mut batch = Vec::new();
    correction(
        &mut batch,
        [20.5, 65.0, 8.5],
        [0.0, 0.0, 0.0],
        last_tick - 2,
    );
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let p = bot.position();
    assert!(
        (p.x - 20.5).abs() < 1e-4 && (p.z - 8.5).abs() < 1e-4,
        "{p:?}"
    );
    assert_eq!(bot.with_state(|s| s.corrections), 1);
}

#[tokio::test(start_paused = true)]
async fn teleport_is_acknowledged_with_handled_teleport_flag() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.drain();
    server.clear();
    let mut batch = Vec::new();
    packet(&mut batch, id::MOVE_PLAYER, |p| {
        put_var_u64(p, RUNTIME_ID);
        put_vec3(p, [12.5, 65.0 + 1.62, 3.5]);
        for v in [10.0f32, 45.0, 45.0] {
            put_f32(p, v);
        }
        p.push(torchflower_bot::protocol::move_mode::TELEPORT);
        p.push(1);
        put_var_u64(p, 0);
        p.extend_from_slice(&3i32.to_le_bytes()); // cause: command
        p.extend_from_slice(&0i32.to_le_bytes());
        put_var_u64(p, 0);
    });
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(150)).await;
    server.drain();
    let pos = bot.position();
    assert!(
        (pos.x - 12.5).abs() < 1e-6 && (pos.z - 3.5).abs() < 1e-6,
        "{pos:?}"
    );
    assert_eq!(bot.rotation(), (45.0, 10.0));
    let inputs = server.auth_inputs();
    let acks = inputs
        .iter()
        .filter(|i| i.flags & torchflower_physics::input_flags::HANDLED_TELEPORT != 0)
        .count();
    assert_eq!(acks, 1, "exactly one input acknowledges the teleport");
}

// ---------------------------------------------------------------------------
// Task 2: modal forms
// ---------------------------------------------------------------------------

fn form_request(batch: &mut Vec<u8>, form_id: u32, json: &str) {
    packet(batch, id::MODAL_FORM_REQUEST, |p| {
        put_var_u32(p, form_id);
        put_string(p, json);
    });
}

fn form_response(payload: &[u8]) -> (u32, Option<String>, Option<u8>) {
    let mut r = WireReader::new(payload);
    let id = r.var_u32().unwrap();
    let data = r.bool().unwrap().then(|| r.string().unwrap().to_string());
    let cancel = r.bool().unwrap().then(|| r.u8().unwrap());
    assert_eq!(r.remaining(), 0);
    (id, data, cancel)
}

#[tokio::test(start_paused = true)]
async fn modal_forms_are_surfaced_and_answered() {
    let (bot, mut server) = spawned_bot().await;
    let mut events = bot.events();
    let menu = r#"{"type":"form","title":"Warps","content":"","buttons":[{"text":"Spawn"},{"text":"Shop"}]}"#;
    let mut batch = Vec::new();
    form_request(&mut batch, 5, menu);
    form_request(
        &mut batch,
        6,
        r#"{"type":"custom_form","title":"Settings","content":[]}"#,
    );
    form_request(
        &mut batch,
        7,
        r#"{"type":"modal","title":"Sure?","content":"","button1":"Yes","button2":"No"}"#,
    );
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;

    match events.recv().await.unwrap() {
        BotEvent::FormRequest { form_id, data } => {
            assert_eq!(form_id, 5);
            assert!(data.contains("Warps"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(bot.open_forms().len(), 3);
    let (id, json) = bot
        .wait_for_form(Duration::from_secs(1), |j| j.contains("Settings"))
        .await
        .unwrap();
    assert_eq!(id, 6);
    assert!(json.contains("custom_form"));

    bot.click_form_button(5, 1).await.unwrap();
    bot.submit_form(6, r#"["TorchBot", 2, true]"#)
        .await
        .unwrap();
    bot.close_form(7).await.unwrap();
    assert_eq!(
        bot.click_form_button(99, 0).await,
        Err(torchflower_bot::BotError::FormNotFound(99))
    );
    tokio::time::sleep(Duration::from_millis(60)).await;
    server.drain();
    let responses: Vec<_> = server
        .payloads(id::MODAL_FORM_RESPONSE)
        .iter()
        .map(|p| form_response(p))
        .collect();
    assert_eq!(
        responses,
        vec![
            (5, Some("1".to_string()), None),
            (6, Some(r#"["TorchBot", 2, true]"#.to_string()), None),
            (7, None, Some(0)),
        ]
    );
    assert!(bot.open_forms().is_empty());

    // Server-side close clears pending forms.
    let mut batch = Vec::new();
    form_request(&mut batch, 8, menu);
    packet(&mut batch, id::CLIENT_BOUND_CLOSE_FORM, |_| {});
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(bot.open_forms().is_empty());
}

/// The form queue is bounded, but a form that *replaces* an existing id must
/// not evict an unrelated one, and answering a form in the middle must keep
/// the remaining ones in arrival order so eviction stays "oldest first".
#[tokio::test(start_paused = true)]
async fn form_queue_replaces_by_id_and_evicts_the_oldest() {
    let (bot, mut server) = spawned_bot().await;
    let mut batch = Vec::new();
    for id in 1..=4 {
        form_request(&mut batch, id, &format!(r#"{{"title":"f{id}"}}"#));
    }
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;
    server.drain();
    assert_eq!(bot.open_forms().len(), 4, "queue is full");
    assert!(
        server.payloads(id::MODAL_FORM_RESPONSE).is_empty(),
        "nothing cancelled yet"
    );

    // Re-sending id 2 replaces its stored copy: no eviction, no cancel.
    let mut batch = Vec::new();
    form_request(&mut batch, 2, r#"{"title":"f2-updated"}"#);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;
    server.drain();
    assert!(
        server.payloads(id::MODAL_FORM_RESPONSE).is_empty(),
        "a replacement must not evict another form"
    );
    let forms = bot.open_forms();
    assert_eq!(forms.len(), 4);
    assert!(forms.iter().any(|f| f.1.contains("f2-updated")));
    // The replacement moves to the back, so id 1 is now the oldest.
    assert_eq!(forms.iter().map(|f| f.0).collect::<Vec<_>>(), [1, 3, 4, 2]);

    // Answering the middle form keeps the rest in order.
    bot.click_form_button(3, 0).await.unwrap();
    assert_eq!(
        bot.open_forms().iter().map(|f| f.0).collect::<Vec<_>>(),
        [1, 4, 2]
    );

    // Filling the queue again evicts the oldest with "user busy".
    let mut batch = Vec::new();
    form_request(&mut batch, 5, r#"{"title":"f5"}"#);
    form_request(&mut batch, 6, r#"{"title":"f6"}"#);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;
    server.drain();
    let cancels: Vec<_> = server
        .payloads(id::MODAL_FORM_RESPONSE)
        .iter()
        .map(|p| form_response(p))
        .filter(|r| r.2 == Some(1))
        .collect();
    assert_eq!(
        cancels,
        vec![(1, None, Some(1))],
        "oldest cancelled as busy"
    );
    assert_eq!(
        bot.open_forms().iter().map(|f| f.0).collect::<Vec<_>>(),
        [4, 2, 5, 6]
    );
}

// ---------------------------------------------------------------------------
// Task 3: hunger and auto-eat
// ---------------------------------------------------------------------------

const BREAD: i32 = 261;

fn hunger(batch: &mut Vec<u8>, health: f32, food: f32) {
    packet(batch, id::UPDATE_ATTRIBUTES, |p| {
        put_var_u64(p, RUNTIME_ID);
        put_var_u32(p, 2);
        for (name, value, max) in [
            ("minecraft:health", health, 20.0f32),
            ("minecraft:player.hunger", food, 20.0),
        ] {
            put_f32(p, 0.0);
            put_f32(p, max);
            put_f32(p, value);
            put_f32(p, 0.0);
            put_f32(p, max);
            put_f32(p, max);
            put_string(p, name);
            put_var_u32(p, 0);
        }
        put_var_u64(p, 0);
    });
}

/// Decodes the use-item / release-item transactions the bot sent:
/// `(transaction type, action, hotbar slot, item network id)`.
fn transactions(server: &Server) -> Vec<(u32, u32, i32, i32)> {
    server
        .payloads(id::INVENTORY_TRANSACTION)
        .iter()
        .map(|p| {
            let mut r = WireReader::new(p);
            r.var_i32().unwrap();
            let kind = r.var_u32().unwrap();
            r.var_u32().unwrap();
            match kind {
                2 => {
                    let action = r.var_u32().unwrap();
                    r.var_u32().unwrap();
                    r.ublock_pos().unwrap();
                    r.var_i32().unwrap();
                    let slot = r.var_i32().unwrap();
                    let item = torchflower_inventory::ItemStack::read_instance(&mut r).unwrap();
                    (kind, action, slot, item.network_id)
                }
                4 => {
                    let action = r.var_u32().unwrap();
                    let slot = r.var_i32().unwrap();
                    let item = torchflower_inventory::ItemStack::read_instance(&mut r).unwrap();
                    (kind, action, slot, item.network_id)
                }
                other => (other, 0, 0, 0),
            }
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn auto_eat_when_hungry_and_restores_slot() {
    let (bot, mut server) = spawned_bot_with(Setup {
        items: vec![("minecraft:bread", BREAD as i16)],
        inventory: vec![(0, 3, 16), (3, 300, 1), (20, BREAD, 5)],
        ..Setup::default()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(bot.inventory().selected_hotbar(), 0);
    server.drain();
    server.clear();

    let mut events = bot.events();
    let mut batch = Vec::new();
    hunger(&mut batch, 20.0, 12.0);
    server.send(batch);
    // Eating takes 32 ticks.
    tokio::time::sleep(Duration::from_millis(50 * 34)).await;
    server.drain();

    // Bread was moved from slot 20 into the selected hotbar slot and held.
    assert!(
        server.count(id::ITEM_STACK_REQUEST) >= 1,
        "swap into hotbar"
    );
    let tx = transactions(&server);
    let clicks: Vec<_> = tx.iter().filter(|t| t.0 == 2 && t.1 == 1).collect();
    assert_eq!(clicks.len(), 2, "click-air to start and to finish: {tx:?}");
    assert!(clicks.iter().all(|t| t.3 == BREAD), "holding bread: {tx:?}");
    assert!(
        tx.iter().any(|t| t.0 == 4 && t.1 == 1),
        "consume release: {tx:?}"
    );
    let started = server
        .auth_inputs()
        .iter()
        .filter(|i| i.flags & torchflower_physics::input_flags::START_USING_ITEM != 0)
        .count();
    assert_eq!(started, 1, "StartUsingItem set once");

    // Server confirms by raising the hunger bar.
    let mut batch = Vec::new();
    hunger(&mut batch, 20.0, 17.0);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut ate = false;
    while let Ok(ev) = events.try_recv() {
        if ev == (BotEvent::Ate { item: BREAD }) {
            ate = true;
        }
    }
    assert!(ate, "Ate event");
    assert_eq!(bot.food(), 17.0);
    assert_eq!(bot.inventory().selected_hotbar(), 0, "slot restored");
    // Not hungry any more: no second eat starts.
    server.drain();
    server.clear();
    tokio::time::sleep(Duration::from_millis(50 * 40)).await;
    server.drain();
    assert!(transactions(&server).is_empty());
}

#[tokio::test(start_paused = true)]
async fn manual_eat_and_full_hunger_rules() {
    let (bot, mut server) = spawned_bot_with(Setup {
        items: vec![("minecraft:bread", BREAD as i16)],
        inventory: vec![(0, 3, 16), (4, BREAD, 2)],
        configure: |c| c.auto_eat = false,
        ..Setup::default()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Food is full (20): normal food is refused immediately.
    let full = bot.eat().await;
    assert!(
        matches!(full, Err(torchflower_bot::BotError::Other(ref m)) if m.contains("not hungry")),
        "{full:?}"
    );

    let mut batch = Vec::new();
    hunger(&mut batch, 20.0, 15.0);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let eater = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.eat().await })
    };
    tokio::time::sleep(Duration::from_millis(50 * 34)).await;
    assert!(!eater.is_finished(), "waits for the server");
    // Confirmation via the eating ActorEvent (Feed, data = id << 16).
    let mut batch = Vec::new();
    packet(&mut batch, id::ACTOR_EVENT, |p| {
        put_var_u64(p, RUNTIME_ID);
        p.push(torchflower_bot::protocol::actor_event::FEED);
        put_var_i32(p, BREAD << 16);
    });
    server.send(batch);
    let res = tokio::time::timeout(Duration::from_secs(1), eater)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(BREAD));
    assert_eq!(bot.inventory().selected_hotbar(), 0);
    server.drain();
    // Slot 4 is already in the hotbar: selected directly, no swap.
    assert_eq!(server.count(id::ITEM_STACK_REQUEST), 0);
}

// ---------------------------------------------------------------------------
// Task 4: dropped items
// ---------------------------------------------------------------------------

fn add_item_actor(
    batch: &mut Vec<u8>,
    runtime_id: u64,
    network_id: i32,
    count: u16,
    pos: [f32; 3],
) {
    packet(batch, id::ADD_ITEM_ACTOR, |p| {
        put_var_i64(p, runtime_id as i64);
        put_var_u64(p, runtime_id);
        put_item(p, 898, network_id, count, 0, 0);
        put_vec3(p, pos);
        put_vec3(p, [0.0; 3]);
        put_var_u32(p, 0); // metadata
        p.push(0); // from fishing
    });
}

fn take_item_actor(batch: &mut Vec<u8>, item: u64) {
    packet(batch, id::TAKE_ITEM_ACTOR, |p| {
        put_var_u64(p, item);
        put_var_u64(p, RUNTIME_ID);
    });
}

/// Plays the server: hands items to the bot once it stands on them.
async fn pickup_server(
    bot: &Bot,
    server: &mut Server,
    items: &[(u64, [f32; 3])],
    ticks: usize,
) -> Vec<u64> {
    let mut taken = Vec::new();
    for _ in 0..ticks {
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.drain();
        let pos = bot.position();
        if std::env::var("TRACE_COLLECT").is_ok() {
            eprintln!(
                "pos {:.2} {:.2} {:.2} ground {}",
                pos.x,
                pos.y,
                pos.z,
                bot.with_state(|s| s.player.on_ground)
            );
        }
        for (id, at) in items {
            if taken.contains(id) {
                continue;
            }
            let d = ((pos.x - at[0] as f64).powi(2) + (pos.z - at[2] as f64).powi(2)).sqrt();
            if d < 1.0 && (pos.y - at[1] as f64).abs() < 1.5 {
                let mut batch = Vec::new();
                take_item_actor(&mut batch, *id);
                server.send(batch);
                taken.push(*id);
            }
        }
        if taken.len() == items.len() {
            break;
        }
    }
    taken
}

#[tokio::test(start_paused = true)]
async fn collect_drops_visits_items_and_counts_pickups() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let items = [(900u64, [11.5f32, 65.1, 8.5]), (901, [8.5, 65.1, 12.5])];
    let mut batch = Vec::new();
    for (id, pos) in &items {
        add_item_actor(&mut batch, *id, 3, 2, *pos);
    }
    // One far item that must be ignored.
    add_item_actor(&mut batch, 902, 3, 1, [14.5, 65.1, 14.5]);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let drops = bot.nearby_drops(5.0);
    assert_eq!(
        drops.iter().map(|e| e.runtime_id).collect::<Vec<_>>(),
        vec![900, 901]
    );
    assert_eq!(drops[0].item, Some((3, 2)));

    let collector = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.collect_drops(5.0).await })
    };
    let taken = pickup_server(&bot, &mut server, &items, 400).await;
    assert_eq!(taken.len(), 2, "bot walked over both items");
    let res = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(2));
    assert_eq!(bot.nearby_drops(5.0).len(), 0);
    assert!(bot.entity(902).is_some(), "far item untouched");
}

#[tokio::test(start_paused = true)]
async fn dig_with_collect_picks_up_the_drop() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let digger = {
        let bot = bot.clone();
        tokio::spawn(async move {
            bot.dig_with(
                BlockPos::new(7, 65, 8),
                torchflower_bot::DigOptions {
                    collect_drops: true,
                },
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50 * 30)).await;
    // Server breaks the block and spawns its drop.
    let air = rid(&bot, "air");
    let mut batch = Vec::new();
    packet(&mut batch, id::UPDATE_BLOCK, |p| {
        put_ublock_pos(p, [7, 65, 8]);
        put_var_u32(p, air);
        put_var_u32(p, 3);
        put_var_u32(p, 0);
    });
    add_item_actor(&mut batch, 950, 3, 1, [7.5, 65.1, 8.5]);
    server.send(batch);
    assert!(!digger.is_finished(), "waits for the drop");
    let taken = pickup_server(&bot, &mut server, &[(950, [7.5, 65.1, 8.5])], 200).await;
    assert_eq!(taken, vec![950]);
    let res = tokio::time::timeout(Duration::from_secs(5), digger)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()));
}

/// The block can be confirmed broken without an `UpdateBlock` that turns it
/// into air (here the server replaces it with another block). That path
/// completes the dig through the timeout fallback, which must still run the
/// requested auto-collect.
#[tokio::test(start_paused = true)]
async fn dig_with_collect_runs_on_the_fallback_completion_path() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let digger = {
        let bot = bot.clone();
        tokio::spawn(async move {
            bot.dig_with(
                BlockPos::new(7, 65, 8),
                torchflower_bot::DigOptions {
                    collect_drops: true,
                },
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50 * 30)).await;
    // The ore is gone, but what replaced it is not air, so no "broken to air"
    // confirmation ever arrives.
    let dirt = rid(&bot, "dirt");
    let mut batch = Vec::new();
    packet(&mut batch, id::UPDATE_BLOCK, |p| {
        put_ublock_pos(p, [7, 65, 8]);
        put_var_u32(p, dirt);
        put_var_u32(p, 3);
        put_var_u32(p, 0);
    });
    add_item_actor(&mut batch, 951, 3, 1, [7.5, 66.1, 8.5]);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(50 * 25)).await;
    assert!(!digger.is_finished(), "the auto-collect still has to run");

    let taken = pickup_server(&bot, &mut server, &[(951, [7.5, 66.1, 8.5])], 200).await;
    assert_eq!(taken, vec![951], "walked over the drop");
    let res = tokio::time::timeout(Duration::from_secs(5), digger)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()));
}

/// Asking for a collect run while a dig is still waiting for its drop must
/// answer the dig (the block *is* broken) and then run the new request.
#[tokio::test(start_paused = true)]
async fn collect_drops_during_a_dig_drop_wait_answers_the_dig() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let digger = {
        let bot = bot.clone();
        tokio::spawn(async move {
            bot.dig_with(
                BlockPos::new(7, 65, 8),
                torchflower_bot::DigOptions {
                    collect_drops: true,
                },
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50 * 30)).await;
    let air = rid(&bot, "air");
    let mut batch = Vec::new();
    packet(&mut batch, id::UPDATE_BLOCK, |p| {
        put_ublock_pos(p, [7, 65, 8]);
        put_var_u32(p, air);
        put_var_u32(p, 3);
        put_var_u32(p, 0);
    });
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!digger.is_finished(), "waiting for the drop to spawn");

    // No drop was spawned; the user asks for a collect run of their own.
    let collector = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.collect_drops(5.0).await })
    };
    let res = tokio::time::timeout(Duration::from_secs(5), digger)
        .await
        .expect("dig answered")
        .unwrap();
    assert_eq!(res, Ok(()), "the block is broken either way");
    let collected = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(collected, Ok(0), "no drops in range");
    server.drain();
}

/// Collection and user navigation share one route slot. Whoever asks last
/// owns it, and the one that loses must be *answered* rather than left with a
/// dropped reply that never resolves.
#[tokio::test(start_paused = true)]
async fn navigation_and_collection_never_drop_each_other_s_reply() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut batch = Vec::new();
    add_item_actor(&mut batch, 960, 3, 1, [11.5, 65.1, 8.5]);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;

    // A route is running; collection takes over and cancels it.
    let nav = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.navigate_to(GoalBlock::new(13, 65, 8)).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let collector = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.collect_drops(5.0).await })
    };
    let res = tokio::time::timeout(Duration::from_secs(5), nav)
        .await
        .expect("navigate answered, not dropped")
        .unwrap();
    assert_eq!(res, Err(torchflower_bot::BotError::Cancelled));

    // Now the other way round: a new route ends the collect run, which
    // reports what it picked up so far instead of hanging.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let nav2 = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.navigate_to(GoalBlock::new(8, 65, 8)).await })
    };
    let collected = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("collect answered, not dropped")
        .unwrap();
    assert_eq!(collected, Ok(0), "nothing was picked up");
    let res = tokio::time::timeout(Duration::from_secs(20), nav2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, Ok(()), "the later route still runs");
    server.drain();
}

/// `stop()` stops every driver-owned movement, so an in-flight collect run is
/// answered and the bot stops walking.
#[tokio::test(start_paused = true)]
async fn stop_ends_an_active_collect_run() {
    let (bot, mut server) = spawned_bot().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut batch = Vec::new();
    add_item_actor(&mut batch, 961, 3, 1, [12.5, 65.1, 8.5]);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let collector = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.collect_drops(6.0).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!collector.is_finished(), "still walking to the item");

    bot.stop().expect("stop sent");
    let res = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("collect answered by stop()")
        .unwrap();
    assert_eq!(res, Ok(0));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let a = bot.position();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let b = bot.position();
    assert!(
        (a.x - b.x).abs() < 0.05 && (a.z - b.z).abs() < 0.05,
        "stopped walking: {a:?} -> {b:?}"
    );
    server.drain();
}

/// Dying while tasks are in flight must answer all of them: the server resets
/// the player, so no confirmation can ever arrive.
#[tokio::test(start_paused = true)]
async fn death_answers_every_pending_task() {
    let (bot, mut server) = spawned_bot_with(Setup {
        inventory: vec![(0, 3, 16), (3, 300, 1), (5, BREAD, 4)],
        items: vec![("minecraft:bread", BREAD as i16)],
        configure: |c| c.auto_eat = false,
        ..Setup::default()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut batch = Vec::new();
    add_item_actor(&mut batch, 962, 3, 1, [12.5, 65.1, 8.5]);
    // Hungry enough for `eat()` to be accepted.
    hunger(&mut batch, 20.0, 10.0);
    server.send(batch);
    tokio::time::sleep(Duration::from_millis(60)).await;

    let digger = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.dig(BlockPos::new(7, 65, 8)).await })
    };
    let collector = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.collect_drops(6.0).await })
    };
    let eater = {
        let bot = bot.clone();
        tokio::spawn(async move { bot.eat().await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut events = bot.events();
    let mut batch = Vec::new();
    hunger(&mut batch, 0.0, 0.0);
    server.send(batch);

    let dig = tokio::time::timeout(Duration::from_secs(5), digger)
        .await
        .expect("dig answered")
        .unwrap();
    assert_eq!(dig, Err(torchflower_bot::BotError::Cancelled));
    let eat = tokio::time::timeout(Duration::from_secs(5), eater)
        .await
        .expect("eat answered")
        .unwrap();
    assert_eq!(eat, Err(torchflower_bot::BotError::Cancelled));
    let collected = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .expect("collect answered")
        .unwrap();
    assert_eq!(collected, Ok(0));
    assert!(bot.with_state(|s| s.dead));
    let mut saw_death = false;
    while let Ok(ev) = events.try_recv() {
        saw_death |= matches!(ev, BotEvent::Death);
    }
    assert!(saw_death, "Death event emitted");
    server.drain();
}

// ---------------------------------------------------------------------------
// Task 5: embedded vanilla palette
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn embedded_palette_needs_no_external_file() {
    let (bot, _server) = spawned_bot_with(Setup {
        custom_palette: false,
        ..Setup::default()
    })
    .await;
    let states = bot.with_state(|s| s.world.registry().len());
    assert_eq!(states, 15845, "full 1.21.130 palette");
    // Real vanilla names resolve and the chunk is decoded against them.
    assert_eq!(
        bot.block_at(BlockPos::new(1, 64, 1)).unwrap().name,
        "minecraft:stone"
    );
    let ore = bot
        .find_block("minecraft:iron_ore", 16)
        .expect("iron ore found by name");
    assert_eq!(ore.pos, BlockPos::new(7, 65, 8));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        bot.with_state(|s| s.player.on_ground),
        "physics sees the real stone floor"
    );
}

// ---------------------------------------------------------------------------
// Protocol version matrix: the same session on newer protocols
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn session_works_on_1_26_protocols() {
    for protocol in [924, 944, 975, 1001] {
        let (bot, mut server) = spawned_bot_with(Setup {
            protocol,
            custom_palette: false,
            ..Setup::default()
        })
        .await;
        assert_eq!(server.protocol, protocol);
        assert_eq!(
            bot.inventory().get(3).map(|i| i.network_id),
            Some(300),
            "inventory decoded on {protocol}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Chat in the protocol's Text layout.
        let mut batch = Vec::new();
        packet(&mut batch, id::TEXT, |p| {
            p.push(0);
            p.push(1);
            if protocol < 924 {
                for c in ["chat", "whisper", "announcement"] {
                    put_string(p, c);
                }
            }
            p.push(text_type::CHAT);
            put_string(p, "Steve");
            put_string(p, "hi");
            put_string(p, "");
            put_string(p, "");
            p.push(0);
        });
        let mut events = bot.events();
        server.send(batch);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let mut got = false;
        while let Ok(ev) = events.try_recv() {
            if let BotEvent::Chat {
                sender, message, ..
            } = ev
            {
                got = sender == "Steve" && message == "hi";
            }
        }
        assert!(got, "chat decoded on {protocol}");

        // Place (UseItem layout differs at 944 and 1001).
        let placer = {
            let bot = bot.clone();
            tokio::spawn(async move { bot.place_block(BlockPos::new(9, 64, 8), 1).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let dirt = rid(&bot, "dirt");
        let mut batch = Vec::new();
        packet(&mut batch, id::UPDATE_BLOCK, |p| {
            if protocol >= 944 {
                torchflower_protocol_core::wire::put_block_pos(p, [9, 65, 8]);
            } else {
                put_ublock_pos(p, [9, 65, 8]);
            }
            put_var_u32(p, dirt);
            put_var_u32(p, 3);
            put_var_u32(p, 0);
        });
        server.send(batch);
        let res = tokio::time::timeout(Duration::from_secs(2), placer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, Ok(()), "place on {protocol}");
        server.drain();
        let tx = server.payloads(id::INVENTORY_TRANSACTION);
        let mut r = WireReader::new(&tx[0]);
        r.var_i32().unwrap();
        if protocol >= 1001 {
            assert_eq!(
                [r.u8().unwrap(), r.u8().unwrap()],
                [0, 1],
                "v2 header on {protocol}"
            );
        }
        bot.disconnect();
    }
}
