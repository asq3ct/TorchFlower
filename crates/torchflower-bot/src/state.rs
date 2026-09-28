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
    /// Server host name or address.
    pub host: String,
    /// Server port (19132 by default for Bedrock).
    pub port: u16,
    /// Player name (taken from the session for authenticated bots).
    pub username: String,
    /// Authentication mode.
    pub auth: Auth,
    /// Protocol version to speak.
    pub protocol: BedrockProtocolOptions,
    /// World window size.
    pub window: WindowConfig,
    /// Chunk radius requested from the server.
    pub chunk_radius: i32,
    /// Maximum tracked entities.
    pub entity_capacity: usize,
    /// Entities further than this (blocks) are evicted (players excepted).
    pub entity_radius: f64,
    /// Optional `canonical_block_states.nbt` overriding the vanilla palette
    /// that is embedded for the negotiated protocol (needed only for servers
    /// with a non-vanilla block palette).
    pub canonical_block_states: Option<Arc<[u8]>>,
    /// Respawn automatically after death.
    pub auto_respawn: bool,
    /// Eat automatically when hungry or hurt (see [`AutoEatConfig`]).
    pub auto_eat: bool,
    /// Thresholds and food preferences for eating.
    pub eat: AutoEatConfig,
    /// How long `connect` waits for the spawn.
    pub spawn_timeout: Duration,
    /// Interaction reach in blocks.
    pub reach: f64,
    /// Movement constants.
    pub physics: PhysicsConfig,
    /// Pathfinder limits and allowed moves.
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
            auto_eat: true,
            eat: AutoEatConfig::default(),
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

/// When and what the bot eats.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoEatConfig {
    /// Eat when health drops below this (half-hearts) and food is not full.
    pub health_below: f32,
    /// Eat when food is at or below this (0–20).
    pub food_at_or_below: f32,
    /// Ticks the item is held in use before consuming (vanilla: 32).
    pub eat_ticks: u32,
    /// Foods the bot may eat, best first. Items not in this list are never
    /// eaten (so rotten flesh, spider eyes, pufferfish… are avoided unless
    /// added explicitly).
    pub foods: Vec<String>,
}

impl Default for AutoEatConfig {
    fn default() -> Self {
        Self {
            health_below: 18.0,
            food_at_or_below: 14.0,
            eat_ticks: 32,
            foods: DEFAULT_FOODS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Default food preference order (best saturation first; golden apples last
/// so they are kept for emergencies unless nothing else is left).
///
/// Only foods that are safe to eat unconditionally are listed. Notably absent
/// are suspicious stew (carries an unknown, possibly harmful effect), raw
/// chicken, rotten flesh, spider eyes, pufferfish and poisonous potatoes;
/// they can still be eaten explicitly with
/// [`Bot::eat_item`](crate::Bot::eat_item) or by naming them in
/// [`AutoEatConfig::foods`].
pub const DEFAULT_FOODS: &[&str] = &[
    "minecraft:cooked_beef",
    "minecraft:cooked_porkchop",
    "minecraft:cooked_mutton",
    "minecraft:cooked_salmon",
    "minecraft:cooked_chicken",
    "minecraft:cooked_rabbit",
    "minecraft:cooked_cod",
    "minecraft:rabbit_stew",
    "minecraft:mushroom_stew",
    "minecraft:beetroot_soup",
    "minecraft:bread",
    "minecraft:baked_potato",
    "minecraft:pumpkin_pie",
    "minecraft:carrot",
    "minecraft:apple",
    "minecraft:melon_slice",
    "minecraft:sweet_berries",
    "minecraft:glow_berries",
    "minecraft:cookie",
    "minecraft:dried_kelp",
    "minecraft:beetroot",
    "minecraft:potato",
    "minecraft:beef",
    "minecraft:porkchop",
    "minecraft:mutton",
    "minecraft:rabbit",
    "minecraft:cod",
    "minecraft:salmon",
    "minecraft:golden_carrot",
    "minecraft:golden_apple",
    "minecraft:enchanted_golden_apple",
];

/// Events emitted by the bot.
#[derive(Debug, Clone, PartialEq)]
pub enum BotEvent {
    /// The bot spawned in the world.
    Spawned,
    /// A chat or system message from someone other than the bot.
    Chat {
        /// Sender name (empty for system messages).
        sender: String,
        /// Message text.
        message: String,
        /// Text type (see [`crate::protocol::text_type`]).
        kind: u8,
    },
    /// Health or hunger changed.
    Health {
        /// Health in half-hearts (0–20).
        health: f32,
        /// Hunger (0–20).
        food: f32,
    },
    /// The bot died.
    Death,
    /// The bot respawned after dying.
    Respawned,
    /// The server moved the bot (teleport, reset or dimension change) to
    /// this feet position.
    Teleported(Vec3),
    /// A block changed.
    BlockUpdate {
        /// Block position.
        pos: BlockPos,
        /// New runtime id.
        runtime_id: u32,
    },
    /// An entity started being tracked (runtime id).
    EntitySpawned(u64),
    /// A tracked entity was removed (runtime id).
    EntityRemoved(u64),
    /// A container window opened (window id).
    WindowOpened(u8),
    /// A container window closed (window id).
    WindowClosed(u8),
    /// The server opened a modal form. `data` is the form JSON; answer with
    /// [`crate::Bot::click_form_button`], [`crate::Bot::submit_form`] or
    /// [`crate::Bot::close_form`].
    FormRequest {
        /// Id to answer with.
        form_id: u32,
        /// Form JSON (`type` is `form`, `modal` or `custom_form`).
        data: String,
    },
    /// The server closed all open forms.
    FormsClosed,
    /// The server corrected the bot's movement (rubber-band). `position` is
    /// the authoritative feet position.
    MovementCorrected {
        /// Authoritative feet position.
        position: Vec3,
        /// The input tick the server corrected.
        tick: u64,
    },
    /// The bot finished eating.
    Ate {
        /// Network id of the food eaten.
        item: i32,
    },
    /// The server kicked the bot (with the reason given).
    Kicked(String),
    /// The connection closed (with the reason).
    Disconnected(String),
    /// A handler registered with [`crate::Bot::on_chat`] returned an error.
    HandlerError(String),
}

/// Complete observable bot state.
pub struct BotState {
    /// The bot's player name.
    pub username: String,
    /// Loaded blocks around the bot.
    pub world: SparseWorld,
    /// Inventory and open container.
    pub inventory: Inventory,
    /// Nearby entities, including dropped items.
    pub entities: EntityTable,
    /// Simulated movement state.
    pub player: PlayerState,
    /// The bot's runtime entity id.
    pub runtime_id: u64,
    /// The bot's unique entity id.
    pub unique_id: i64,
    /// Game mode (0 survival, 1 creative, 2 adventure, 3 spectator).
    pub game_mode: i32,
    /// Health in half-hearts (0–20).
    pub health: f32,
    /// Hunger (0–20).
    pub food: f32,
    /// World time from `SetTime`.
    pub time_of_day: i32,
    /// True once `PlayStatus::PlayerSpawn` was received.
    pub spawned: bool,
    /// True between death and respawn.
    pub dead: bool,
    /// Whether the server runs block breaking server-side (`StartGame`).
    pub server_authoritative_breaking: bool,
    /// Item names and ids from the server.
    pub items: Arc<ItemRegistry>,
    /// Crafting recipes from the server.
    pub recipes: Arc<RecipeBook>,
    /// Negotiated protocol version.
    pub protocol: i32,
    /// Forms opened by the server and not yet answered: `(form_id, json)`.
    pub open_forms: Vec<(u32, String)>,
    /// Number of movement corrections received since spawn.
    pub corrections: u64,
}

impl BotState {
    /// Fresh state.
    pub fn new(config: &BotConfig, protocol: i32) -> Self {
        let registry = shared_registry(
            config.canonical_block_states.as_ref(),
            protocol,
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
            open_forms: Vec::new(),
            corrections: 0,
        }
    }

    /// Approximate heap bytes owned by this bot (excluding registries,
    /// item/recipe tables and block palettes shared across bots).
    pub fn heap_bytes(&self) -> usize {
        self.world.heap_bytes()
            + self.inventory.heap_bytes()
            + self.entities.heap_bytes()
            + self.username.capacity()
            + self
                .open_forms
                .iter()
                .map(|(_, json)| json.capacity())
                .sum::<usize>()
    }
}

type RegistryCache = Mutex<HashMap<(usize, i32, bool), Weak<BlockRegistry>>>;

/// Returns the block registry for a connection, shared by every bot with
/// the same palette source, protocol and runtime-id mode.
///
/// * With `states` (a `canonical_block_states.nbt`), that palette is used.
/// * Otherwise the vanilla palette embedded for `protocol` is used, so no
///   external files are needed.
/// * [`BlockRegistry::fallback`] is only used if both fail to decode.
pub fn shared_registry(
    states: Option<&Arc<[u8]>>,
    protocol: i32,
    mode: RuntimeIdMode,
) -> Arc<BlockRegistry> {
    static CACHE: OnceLock<RegistryCache> = OnceLock::new();
    let (key_ptr, key_protocol) = match states {
        Some(s) => (s.as_ptr() as usize, 0),
        None => (
            0,
            torchflower_world::embedded_palette_for(protocol)
                .map(|p| p.first_protocol)
                .unwrap_or(0),
        ),
    };
    let key = (key_ptr, key_protocol, mode == RuntimeIdMode::Hashed);
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
            .or_else(|| BlockRegistry::vanilla(protocol, mode).ok())
            .unwrap_or_else(|| BlockRegistry::fallback(mode)),
    );
    guard.retain(|_, w| w.strong_count() > 0);
    guard.insert(key, Arc::downgrade(&registry));
    registry
}
