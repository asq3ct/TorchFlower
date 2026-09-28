//! Memory budget checks.
//!
//! * `full_window_stays_under_one_mebibyte` fills a bot's state to its
//!   worst case (every sub-chunk of the window loaded with noisy 40-state
//!   palettes, a full entity table, forms, a full inventory) and checks the
//!   component accounting.
//! * `live_bots_allocate_under_one_mebibyte_each` counts *real* heap
//!   allocations with a global allocator while running spawned bots over the
//!   in-memory transport, so everything a bot owns (driver task, channels,
//!   buffers, state) is included. The shared block registry is created
//!   before measuring, because it is allocated once per process and shared
//!   by every bot on that protocol.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use torchflower_bot::protocol::id;
use torchflower_bot::transport::memory::pair;
use torchflower_bot::{shared_registry, Bot, BotConfig, BotState};
use torchflower_inventory::{window_id, ItemStack};
use torchflower_physics::Vec3;
use torchflower_protocol_core::wire::{
    begin_packet, end_packet, put_f32, put_string, put_var_i32, put_var_i64, put_var_u32,
    put_var_u64, put_vec3,
};
use torchflower_world::palette::local_index;
use torchflower_world::{BlockPos, BlockRegistry, RuntimeIdMode, SubChunk};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const MIB: usize = 1024 * 1024;

#[test]
fn full_window_stays_under_one_mebibyte() {
    let cfg = BotConfig::offline("fake", 19132, "MemoryBot");
    let mut state = BotState::new(&cfg, 898);
    // 40 distinct "block states" per sub-chunk (6 bits per block) — worse
    // than typical overworld terrain.
    let registry = Arc::new(BlockRegistry::from_states(
        (0..64).map(|i| (format!("minecraft:test_block_{i}"), None)),
        RuntimeIdMode::Sequential,
    ));
    state.world = torchflower_world::SparseWorld::new(registry, cfg.window);
    state.world.set_center(BlockPos::new(8, 64, 8));
    let mut seed = 12345u32;
    for cx in -2..=2 {
        for cz in -2..=2 {
            for sy in -4..=19 {
                let mut sub = SubChunk::uniform(0);
                let layer = sub.layers[0].as_mut().unwrap();
                for i in 0..4096 {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    let (x, y, z) = ((i >> 8) as u8, (i & 15) as u8, ((i >> 4) & 15) as u8);
                    layer.set(local_index(x, y, z), (seed >> 16) % 40);
                }
                state.world.insert_sub_chunk(cx, sy, cz, sub);
            }
        }
    }
    for i in 0..cfg.entity_capacity as u64 {
        state.entities.spawn(
            Vec3::ZERO,
            i + 1,
            i as i64 + 1,
            if i % 3 == 0 {
                "minecraft:item"
            } else {
                "minecraft:zombie"
            },
            if i % 8 == 0 {
                Some("SomePlayerName")
            } else {
                None
            },
            Vec3::new(i as f64 % 20.0, 64.0, 3.0),
            Vec3::ZERO,
            0.0,
            0.0,
            (i % 3 == 0).then_some((3, 64)),
        );
    }
    // Every inventory slot holds an item with typical extra data.
    let items: Vec<ItemStack> = (0..36)
        .map(|i| ItemStack {
            network_id: 1 + i,
            count: 64,
            stack_id: 100 + i,
            extra: vec![0u8; 48].into(),
            ..Default::default()
        })
        .collect();
    state.inventory.apply_content(window_id::INVENTORY, items);
    // The maximum number of retained forms, each a large menu.
    for f in 0..4 {
        state.open_forms.push((f, "x".repeat(16 * 1024)));
    }

    let (hot, cold) = state.world.sub_chunk_counts();
    assert_eq!(hot + cold, 25 * 24);
    let bytes = state.heap_bytes();
    let struct_bytes = std::mem::size_of::<BotState>();
    eprintln!(
        "hot={hot} cold={cold} world={} inventory={} entities={} forms={} total_heap={bytes} struct={struct_bytes}",
        state.world.heap_bytes(),
        state.inventory.heap_bytes(),
        state.entities.heap_bytes(),
        4 * 16 * 1024,
    );
    assert!(bytes + struct_bytes < MIB, "state uses {bytes} bytes");
}

fn packet(batch: &mut Vec<u8>, packet_id: u32, body: impl FnOnce(&mut Vec<u8>)) {
    let mark = begin_packet(batch, packet_id);
    body(batch);
    end_packet(batch, mark);
}

/// A realistic column: stone up to y=62, dirt, grass-level air above, with
/// some ore; sent as inline sub-chunks like BDS does for nearby columns.
fn level_chunk(bot: &Bot, cx: i32, cz: i32) -> Vec<u8> {
    let (air, stone, dirt, ore) = bot.with_state(|s| {
        let r = s.world.registry();
        let id = |n: &str| r.runtime_ids_for_name(n)[0];
        (id("air"), id("stone"), id("dirt"), id("iron_ore"))
    });
    let mut body = Vec::new();
    for sy in -4i8..=4 {
        let mut sub = SubChunk::uniform(if sy < 3 { stone } else { air });
        if sy >= 3 {
            let layer = sub.layers[0].as_mut().unwrap();
            for x in 0..16 {
                for z in 0..16 {
                    let top = if sy == 3 { 15 } else { 0 };
                    if sy == 3 {
                        for y in 0..=top {
                            layer.set(local_index(x, y, z), if y > 12 { dirt } else { stone });
                        }
                    }
                }
            }
        } else if sy % 2 == 0 {
            let layer = sub.layers[0].as_mut().unwrap();
            for i in (0..4096).step_by(97) {
                layer.set(i, ore);
            }
        }
        sub.encode(sy, &mut body);
    }
    let mut batch = Vec::new();
    packet(&mut batch, id::LEVEL_CHUNK, |p| {
        put_var_i32(p, cx);
        put_var_i32(p, cz);
        put_var_i32(p, 0);
        put_var_u32(p, 9);
        p.push(0);
        put_var_u32(p, body.len() as u32);
        p.extend_from_slice(&body);
    });
    batch
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn live_bots_allocate_under_one_mebibyte_each() {
    const BOTS: usize = 16;
    // Shared, per-process data: built once before measuring.
    let registry = shared_registry(None, 898, RuntimeIdMode::Sequential);
    let _hashed = shared_registry(None, 898, RuntimeIdMode::Hashed);
    tokio::time::sleep(Duration::from_millis(1)).await;

    let before = LIVE.load(Ordering::Relaxed);
    let mut bots = Vec::new();
    let mut servers = Vec::new();
    for n in 0..BOTS {
        let (transport, server) = pair(898);
        let mut cfg = BotConfig::offline("fake", 19132, format!("MemBot{n}"));
        cfg.spawn_timeout = Duration::from_secs(5);
        let bot = Bot::start(transport, cfg, "0".into());
        let mut batch = Vec::new();
        packet(&mut batch, id::START_GAME, |p| {
            put_var_i64(p, 7);
            put_var_u64(p, 7);
            put_var_i32(p, 0);
            put_vec3(p, [8.5, 64.0 + 1.62, 8.5]);
            put_f32(p, 0.0);
            put_f32(p, 0.0);
            p.extend_from_slice(&[0u8; 10]);
            put_string(p, "");
            put_var_i32(p, 0);
        });
        server.to_bot.send(batch).unwrap();
        bots.push(bot);
        servers.push(server);
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    for (bot, server) in bots.iter().zip(&servers) {
        // The whole 5x5 window, then spawn.
        for cx in -2..=2 {
            for cz in -2..=2 {
                server.to_bot.send(level_chunk(bot, cx, cz)).unwrap();
            }
        }
        let mut batch = Vec::new();
        packet(&mut batch, id::PLAY_STATUS, |p| {
            p.extend_from_slice(&3i32.to_be_bytes())
        });
        server.to_bot.send(batch).unwrap();
    }
    for bot in &bots {
        bot.wait_for_spawn().await.unwrap();
    }
    // Run for 10 simulated seconds (200 ticks of PlayerAuthInput each),
    // draining what the bots send like a server would.
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        for server in &mut servers {
            while server.from_bot.try_recv().is_ok() {}
        }
    }
    let loaded: Vec<usize> = bots
        .iter()
        .map(|b| b.with_state(|s| s.world.sub_chunk_counts().0 + s.world.sub_chunk_counts().1))
        .collect();
    // 25 columns x 7 non-air sub-chunks (-4..=2 stone, 3 = surface).
    assert!(
        loaded.iter().all(|n| *n == 25 * 8),
        "chunks loaded: {loaded:?}"
    );
    let after = LIVE.load(Ordering::Relaxed);
    let per_bot = (after - before).max(0) as usize / BOTS;
    eprintln!("sub-chunks stored per bot: {:?}", &loaded[..2]);
    eprintln!(
        "live bots: {BOTS}, allocated {} bytes total, {per_bot} bytes per bot, shared registry {} bytes (once per process)",
        after - before,
        registry.heap_bytes()
    );
    assert!(per_bot < MIB, "each bot allocates {per_bot} bytes");
    drop(bots);
}
