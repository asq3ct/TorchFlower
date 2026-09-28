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

async fn spawned_bot() -> (Bot, Server) {
    let (transport, server) = pair(PROTOCOL);
    let mut cfg = BotConfig::offline("fake", 19132, "TestBot");
    cfg.canonical_block_states = Some(canonical_states());
    cfg.spawn_timeout = Duration::from_secs(5);
    let bot = Bot::start(transport, cfg, "0".into());
    let server = Server {
        inner: server,
        sent: Vec::new(),
        typed: Vec::new(),
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
    packet(&mut batch, id::ITEM_REGISTRY, |p| {
        put_var_u32(p, 2);
        for (name, nid) in [("minecraft:stone_pickaxe", 300i16), ("minecraft:dirt", 3)] {
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
    let mut batch = Vec::new();
    packet(&mut batch, id::INVENTORY_CONTENT, |p| {
        put_var_u32(p, 0);
        put_var_u32(p, 36);
        for slot in 0..36 {
            if slot == 0 {
                put_var_i32(p, 3);
                p.extend_from_slice(&16u16.to_le_bytes());
                put_var_u32(p, 0);
                p.push(1);
                put_var_i32(p, 56);
                put_var_i32(p, 1234);
                put_var_u32(p, 10);
                p.extend_from_slice(&[0; 10]);
            } else if slot == 3 {
                put_var_i32(p, 300);
                p.extend_from_slice(&1u16.to_le_bytes());
                put_var_u32(p, 0);
                p.push(1);
                put_var_i32(p, 55);
                put_var_i32(p, 0);
                put_var_u32(p, 10);
                p.extend_from_slice(&[0; 10]);
            } else {
                put_var_i32(p, 0);
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
    assert_eq!((held.network_id, held.stack_id), (3, 56));
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
