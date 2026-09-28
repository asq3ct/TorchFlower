//! Pathfinding for TorchFlower bots.
//!
//! [`find_path`] runs a node- and time-bounded A* over feet positions using
//! walk, diagonal, ascend, descend (≤ `max_drop`), parkour (1–2 block gaps),
//! swim/climb, bridge, pillar and dig moves. [`PathFollower`] converts the
//! resulting [`Step`]s into physics [`Controls`](torchflower_physics::Controls)
//! each tick and requests dig/place actions from the bot.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod follow;
pub mod goal;
pub mod search;

pub use follow::{FollowAction, FollowOutput, FollowStatus, PathFollower};
pub use goal::{Goal, GoalBlock, GoalGetToBlock, GoalNear, GoalXZ, GoalY};
pub use search::{
    find_path, MoveKind, PathOptions, PathResult, PathStatus, PathWorld, Step, WorldView,
};
