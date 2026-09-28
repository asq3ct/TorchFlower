//! Bot errors.

use torchflower_inventory::CraftError;

/// Errors returned by bot operations.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum BotError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("bot is disconnected")]
    Disconnected,
    #[error("timed out: {0}")]
    Timeout(&'static str),
    #[error("block at {0:?} is not loaded")]
    NotLoaded([i32; 3]),
    #[error("target is out of reach ({0:.2} blocks)")]
    OutOfReach(f64),
    #[error("no line of sight to target")]
    NoLineOfSight,
    #[error("block cannot be broken")]
    Unbreakable,
    #[error("nothing to dig at target")]
    NothingToDig,
    #[error("target position is occupied")]
    Occupied,
    #[error("no item in hand")]
    EmptyHand,
    #[error("item not found: {0}")]
    ItemNotFound(String),
    #[error("server rejected the action: {0}")]
    Rejected(String),
    #[error("no path to goal")]
    NoPath,
    #[error("action was cancelled")]
    Cancelled,
    #[error("crafting failed: {0:?}")]
    Craft(CraftError),
    #[error("entity not found")]
    EntityNotFound,
    #[error("{0}")]
    Other(String),
}

/// Result alias.
pub type BotResult<T> = Result<T, BotError>;
