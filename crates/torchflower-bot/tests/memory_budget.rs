//! Verifies the per-bot state stays below 1 MiB with a fully loaded window.

use torchflower_bot::{BotConfig, BotState};
use torchflower_physics::Vec3;
use torchflower_world::palette::local_index;
use torchflower_world::{BlockPos, BlockRegistry, RuntimeIdMode, SubChunk};

#[test]
fn full_window_stays_under_one_mebibyte() {
    let cfg = BotConfig::offline("fake", 19132, "MemoryBot");
    let mut state = BotState::new(&cfg, 898);
    // 40 distinct "block states" per sub-chunk (6 bits per block on the
    // wire, 6 after repacking) — worse than typical overworld terrain.
    let registry = std::sync::Arc::new(BlockRegistry::from_states(
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
            "minecraft:zombie",
            if i % 8 == 0 {
                Some("SomePlayerName")
            } else {
                None
            },
            Vec3::new(i as f64 % 20.0, 64.0, 3.0),
            Vec3::ZERO,
            0.0,
            0.0,
            None,
        );
    }
    let (hot, cold) = state.world.sub_chunk_counts();
    assert_eq!(hot + cold, 25 * 24);
    let bytes = state.heap_bytes();
    let struct_bytes = std::mem::size_of::<BotState>();
    eprintln!(
        "hot={hot} cold={cold} world={} inventory={} entities={} total_heap={bytes} struct={struct_bytes}",
        state.world.heap_bytes(),
        state.inventory.heap_bytes(),
        state.entities.heap_bytes()
    );
    assert!(
        bytes + struct_bytes < 1024 * 1024,
        "state uses {bytes} bytes"
    );
}
