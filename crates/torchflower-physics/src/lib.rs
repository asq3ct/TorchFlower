//! Headless player physics for TorchFlower bots.
//!
//! [`Physics::tick`] advances a [`PlayerState`] by one 50 ms tick against any
//! [`CollisionWorld`] (implemented for [`torchflower_world::SparseWorld`]),
//! and [`InputTracker`] turns the result into `PlayerAuthInput` flags.
//! Nothing here allocates per tick.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod input;
pub mod math;
pub mod player;

pub use input::{flags as input_flags, InputSnapshot, InputTracker};
pub use math::{Aabb, Vec3};
pub use player::{
    look_angles, sanitize_velocity, CollisionWorld, Controls, MovementEffects, Physics,
    PhysicsConfig, PlayerState, TickOutcome, MAX_NETWORK_VELOCITY,
};
