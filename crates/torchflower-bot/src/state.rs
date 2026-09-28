//! Bot configuration, observable state and events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use torchflower_engine::auth::ProvisionedBedrockSession;
use torchflower_engine::bedrock::protocol_adapter::BedrockProtocolOptions;
use torchflower_inventory::{Inventory, ItemRegistry, RecipeBook};
use torchflower_pathfinder::PathOptions;
use torchflower_physics::{PhysicsConfig, PlayerState, Vec3};
use torchflower_world::{BlockPos, BlockRegistry, RuntimeIdMode, SparseWorld, WindowConfig};

use crate::entity::EntityTable;

/// How the bot authenticates.
#[derive(Debug, Clone)]
pub enum Auth {
    /// Unauthenticated (offline-mode servers only).
    Offline,
    /// A session provisioned by the engine's auth flow.
    Session(Box<ProvisionedBedrockSession>),
}

/// Bot configuration.
#[derive(Debug, Clone)]
pub struct BotConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: Auth,
    pub protocol: BedrockProtocolOptions,
    /// World window size.
    pub window: WindowConfig,
    /// Chunk radius requested from the server.
    pub chunk_radius: i32,
    /// Maximum tracked entities.
    pub entity_capacity: usize,
    /// Entities further than this (blocks) are evicted (players excepted).
    pub entity_radius: f64,
    /// Optional `canonical_block_states.nbt` for the server version.
    pub canonical_block_states: Option<Arc<[u8]>>,
    /// Respawn automatically after death.
    pub auto_respawn: bool,
    /// How long `connect` waits for the spawn.
    pub spawn_timeout: Duration,
    /// Interaction reach in blocks.
    pub reach: f64,
    pub physics: PhysicsConfig,
    pub path: PathOptions,
}

impl BotConfig {
    /// Offline configuration with defaults.
    pub fn offline(host: impl Into<String>, port: u16, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            auth: Auth::Offline,
            protocol: BedrockProtocolOptions::from_env_or_default().unwrap_or_else(|_| {
                BedrockProtocolOptions::from_config(898).expect("valid protocol")
            }),
            window: WindowConfig::default(),
            chunk_radius: 4,
            entity_capacity: 48,
            entity_radius: 48.0,
            canonical_block_states: None,
            auto_respawn: true,
            spawn_timeout: Duration::from_secs(30),
            reach: 5.0,
            physics: PhysicsConfig::default(),
            path: PathOptions {
                timeout: Duration::from_millis(25),
                ..PathOptions::default()
            },
        }
    }

    /// Authenticated configuration.
    pub fn with_session(mut self, session: ProvisionedBedrockSession) -> Self {
        self.username = session.chain.display_name.clone();
        self.auth = Auth::Session(Box::new(session));
        self
    }
}

/// Events emitted by the bot.
#[derive(Debug, Clone, PartialEq)]
pub enum BotEvent {
    Spawned,
    Chat {
        sender: String,
        message: String,
        kind: u8,
    },
    Health {
        health: f32,
        food: f32,
    },
    Death,
    Respawned,
    Teleported(Vec3),
    BlockUpdate {
        pos: BlockPos,
        runtime_id: u32,
    },
    EntitySpawned(u64),
    EntityRemoved(u64),
    WindowOpened(u8),
    WindowClosed(u8),
    Kicked(String),
    Disconnected(String),
    HandlerError(String),
}

/// Complete observable bot state.
pub struct BotState {
    pub username: String,
    pub world: SparseWorld,
    pub inventory: Inventory,
    pub entities: EntityTable,
    pub player: PlayerState,
    pub runtime_id: u64,
    pub unique_id: i64,
    pub game_mode: i32,
    pub health: f32,
    pub food: f32,
    pub time_of_day: i32,
    pub spawned: bool,
    pub dead: bool,
    pub server_authoritative_breaking: bool,
    pub items: Arc<ItemRegistry>,
    pub recipes: Arc<RecipeBook>,
    pub protocol: i32,
}

impl BotState {
    /// Fresh state.
    pub fn new(config: &BotConfig, protocol: i32) -> Self {
        let registry = shared_registry(
            config.canonical_block_states.as_ref(),
            RuntimeIdMode::Hashed,
        );
        Self {
            username: config.username.clone(),
            world: SparseWorld::new(registry, config.window),
            inventory: Inventory::default(),
            entities: EntityTable::new(config.entity_capacity, config.entity_radius),
            player: PlayerState::new(Vec3::ZERO),
            runtime_id: 0,
            unique_id: 0,
            game_mode: 0,
            health: 20.0,
            food: 20.0,
            time_of_day: 0,
            spawned: false,
            dead: false,
            server_authoritative_breaking: true,
            items: Arc::new(ItemRegistry::default()),
            recipes: Arc::new(RecipeBook::default()),
            protocol,
        }
    }

    /// Approximate heap bytes owned by this bot (excluding registries,
    /// item/recipe tables and block palettes shared across bots).
    pub fn heap_bytes(&self) -> usize {
        self.world.heap_bytes()
            + self.inventory.heap_bytes()
            + self.entities.heap_bytes()
            + self.username.capacity()
    }
}

type RegistryCache = Mutex<HashMap<(usize, bool), Weak<BlockRegistry>>>;

/// Returns a block registry shared by all bots using the same palette bytes
/// and runtime-id mode.
pub fn shared_registry(states: Option<&Arc<[u8]>>, mode: RuntimeIdMode) -> Arc<BlockRegistry> {
    static CACHE: OnceLock<RegistryCache> = OnceLock::new();
    let key = (
        states.map(|s| s.as_ptr() as usize).unwrap_or(0),
        mode == RuntimeIdMode::Hashed,
    );
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(hit) = guard.get(&key).and_then(Weak::upgrade) {
        return hit;
    }
    let registry = Arc::new(
        states
            .and_then(|s| BlockRegistry::from_canonical_nbt(s, mode).ok())
            .unwrap_or_else(|| BlockRegistry::fallback(mode)),
    );
    guard.retain(|_, w| w.strong_count() > 0);
    guard.insert(key, Arc::downgrade(&registry));
    registry
}
