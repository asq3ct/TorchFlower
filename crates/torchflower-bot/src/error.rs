//! Bot errors.

use torchflower_inventory::CraftError;

/// Errors returned by bot operations.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum BotError {
    /// Connecting or sending failed.
    #[error("transport error: {0}")]
    Transport(String),
    /// A packet could not be decoded.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The bot's connection is closed.
    #[error("bot is disconnected")]
    Disconnected,
    /// An action did not complete in time.
    #[error("timed out: {0}")]
    Timeout(&'static str),
    /// Block data at the position has not been received.
    #[error("block at {0:?} is not loaded")]
    NotLoaded([i32; 3]),
    /// The target is further than the interaction reach.
    #[error("target is out of reach ({0:.2} blocks)")]
    OutOfReach(f64),
    /// Another block is in the way.
    #[error("no line of sight to target")]
    NoLineOfSight,
    /// The block cannot be broken in survival (bedrock, barriers, ...).
    #[error("block cannot be broken")]
    Unbreakable,
    /// The target is air or a liquid.
    #[error("nothing to dig at target")]
    NothingToDig,
    /// The target position is already occupied.
    #[error("target position is occupied")]
    Occupied,
    /// The action needs a held item.
    #[error("no item in hand")]
    EmptyHand,
    /// No such item is known or in the inventory.
    #[error("item not found: {0}")]
    ItemNotFound(String),
    /// The server refused or reverted the action.
    #[error("server rejected the action: {0}")]
    Rejected(String),
    /// The pathfinder could not reach the goal.
    #[error("no path to goal")]
    NoPath,
    /// The action was replaced or stopped before it finished.
    #[error("action was cancelled")]
    Cancelled,
    /// No crafting plan could be made.
    #[error("crafting failed: {0:?}")]
    Craft(CraftError),
    /// The entity is not tracked (despawned or out of range).
    #[error("entity not found")]
    EntityNotFound,
    /// There is no unanswered form with this id.
    #[error("no open form with id {0}")]
    FormNotFound(u32),
    /// Any other failure.
    #[error("{0}")]
    Other(String),
}

/// Result alias.
pub type BotResult<T> = Result<T, BotError>;
