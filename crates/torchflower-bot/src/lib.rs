//! High-level, Mineflayer-style Bedrock bot API for TorchFlower.
//!
//! ```no_run
//! use torchflower_bot::{Bot, BotConfig, GoalNear};
//!
//! # async fn run() -> torchflower_bot::BotResult<()> {
//! let bot = Bot::connect(BotConfig::offline("127.0.0.1", 19132, "Miner")).await?;
//! bot.on_chat(|bot, _sender, message| async move {
//!     if message == "!mine iron" {
//!         if let Some(target) = bot.find_block("minecraft:iron_ore", 32) {
//!             bot.navigate_to(torchflower_bot::GoalGetToBlock(target.pos)).await?;
//!             bot.dig(target.pos).await?;
//!             bot.chat("Iron mined!").await?;
//!         }
//!     }
//!     Ok(())
//! });
//! # let _ = GoalNear::new(torchflower_bot::BlockPos::new(0, 64, 0), 3);
//! # Ok(()) }
//! ```
//!
//! Each bot runs one Tokio task that owns the connection, decodes packets
//! into a memory-bounded [`BotState`], and sends one `PlayerAuthInput` every
//! 50 ms from the physics simulation.

#![forbid(unsafe_code)]

mod bot;
mod driver;
pub mod entity;
pub mod error;
pub mod protocol;
pub mod state;
pub mod transport;

pub use bot::{Block, Bot, TorchFlower};
pub use entity::{Entity, EntityTable};
pub use error::{BotError, BotResult};
pub use state::{shared_registry, Auth, BotConfig, BotEvent, BotState};
pub use transport::{EngineTransport, Transport};

pub use torchflower_inventory::{Hand, Inventory, ItemStack, Window};
pub use torchflower_pathfinder::{
    Goal, GoalBlock, GoalGetToBlock, GoalNear, GoalXZ, GoalY, PathOptions,
};
pub use torchflower_physics::{Controls, Vec3};
pub use torchflower_world::{BlockPos, WindowConfig};
