//! Sparse, memory-bounded Bedrock world model for TorchFlower bots.
//!
//! * [`BlockRegistry`] maps network runtime ids to block states (sequential
//!   or hashed ids) and is shared by all bots of a protocol version.
//! * [`SparseWorld`] keeps only a small sliding window of chunk columns
//!   around the bot, with full paletted data near the bot and 1-bit collision
//!   masks further away.
//! * [`chunk`] decodes `LevelChunk` / `SubChunk` packets and encodes
//!   `SubChunkRequest`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod block;
mod block_table;
pub mod chunk;
pub mod palette;
pub mod palette_data;
mod palette_index;
pub mod raycast;
pub mod registry;
pub mod world;

pub use block::{
    block_info, can_harvest, classify, dig_ticks, BlockFlags, BlockInfo, DigContext, Material,
    Shape, Tool, ToolKind, ToolTier,
};
pub use chunk::{LevelChunk, SubChunk, SubChunkMode, SubChunkResult};
pub use palette::PalettedStorage;
pub use palette_data::{embedded_palette_for, embedded_palettes, EmbeddedPalette, PaletteError};
pub use raycast::RaycastHit;
pub use registry::{network_block_hash, BlockRef, BlockRegistry, RuntimeIdMode};
pub use world::{face_normal, BlockPos, ChunkInsert, SparseWorld, WindowConfig};
