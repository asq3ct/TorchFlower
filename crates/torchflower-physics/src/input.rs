//! `PlayerAuthInput` input-flag generation.
//!
//! Bit positions follow the 1.21.x `InputFlag` enumeration (with
//! `StartJumping` at bit 31), as used by protocol 766–975.

/// PlayerAuthInput input flag bits.
pub mod flags {
    /// `Ascend` input flag.
    pub const ASCEND: u128 = 1 << 0;
    /// `Descend` input flag.
    pub const DESCEND: u128 = 1 << 1;
    /// `JumpDown` input flag.
    pub const JUMP_DOWN: u128 = 1 << 3;
    /// `SprintDown` input flag.
    pub const SPRINT_DOWN: u128 = 1 << 4;
    /// `Jumping` input flag.
    pub const JUMPING: u128 = 1 << 6;
    /// `AutoJumpingInWater` input flag.
    pub const AUTO_JUMPING_IN_WATER: u128 = 1 << 7;
    /// `Sneaking` input flag.
    pub const SNEAKING: u128 = 1 << 8;
    /// `SneakDown` input flag.
    pub const SNEAK_DOWN: u128 = 1 << 9;
    /// `Up` input flag.
    pub const UP: u128 = 1 << 10;
    /// `Down` input flag.
    pub const DOWN: u128 = 1 << 11;
    /// `Left` input flag.
    pub const LEFT: u128 = 1 << 12;
    /// `Right` input flag.
    pub const RIGHT: u128 = 1 << 13;
    /// `UpLeft` input flag.
    pub const UP_LEFT: u128 = 1 << 14;
    /// `UpRight` input flag.
    pub const UP_RIGHT: u128 = 1 << 15;
    /// `WantUp` input flag.
    pub const WANT_UP: u128 = 1 << 16;
    /// `WantDown` input flag.
    pub const WANT_DOWN: u128 = 1 << 17;
    /// `Sprinting` input flag.
    pub const SPRINTING: u128 = 1 << 20;
    /// `StartSprinting` input flag.
    pub const START_SPRINTING: u128 = 1 << 25;
    /// `StopSprinting` input flag.
    pub const STOP_SPRINTING: u128 = 1 << 26;
    /// `StartSneaking` input flag.
    pub const START_SNEAKING: u128 = 1 << 27;
    /// `StopSneaking` input flag.
    pub const STOP_SNEAKING: u128 = 1 << 28;
    /// `StartSwimming` input flag.
    pub const START_SWIMMING: u128 = 1 << 29;
    /// `StopSwimming` input flag.
    pub const STOP_SWIMMING: u128 = 1 << 30;
    /// `StartJumping` input flag.
    pub const START_JUMPING: u128 = 1 << 31;
    /// `StartGliding` input flag.
    pub const START_GLIDING: u128 = 1 << 32;
    /// `StopGliding` input flag.
    pub const STOP_GLIDING: u128 = 1 << 33;
    /// `PerformItemInteraction` input flag.
    pub const PERFORM_ITEM_INTERACTION: u128 = 1 << 34;
    /// `PerformBlockActions` input flag.
    pub const PERFORM_BLOCK_ACTIONS: u128 = 1 << 35;
    /// `PerformItemStackRequest` input flag.
    pub const PERFORM_ITEM_STACK_REQUEST: u128 = 1 << 36;
    /// `HandledTeleport` input flag.
    pub const HANDLED_TELEPORT: u128 = 1 << 37;
    /// `Emoting` input flag.
    pub const EMOTING: u128 = 1 << 38;
    /// `MissedSwing` input flag.
    pub const MISSED_SWING: u128 = 1 << 39;
    /// `StartCrawling` input flag.
    pub const START_CRAWLING: u128 = 1 << 40;
    /// `StopCrawling` input flag.
    pub const STOP_CRAWLING: u128 = 1 << 41;
    /// `StartFlying` input flag.
    pub const START_FLYING: u128 = 1 << 42;
    /// `StopFlying` input flag.
    pub const STOP_FLYING: u128 = 1 << 43;
    /// `ClientAckServerData` input flag.
    pub const CLIENT_ACK_SERVER_DATA: u128 = 1 << 44;
    /// `ClientPredictedVehicle` input flag.
    pub const CLIENT_PREDICTED_VEHICLE: u128 = 1 << 45;
    /// `BlockBreakingDelayEnabled` input flag.
    pub const BLOCK_BREAKING_DELAY_ENABLED: u128 = 1 << 48;
    /// `HorizontalCollision` input flag.
    pub const HORIZONTAL_COLLISION: u128 = 1 << 49;
    /// `VerticalCollision` input flag.
    pub const VERTICAL_COLLISION: u128 = 1 << 50;
    /// `DownLeft` input flag.
    pub const DOWN_LEFT: u128 = 1 << 51;
    /// `DownRight` input flag.
    pub const DOWN_RIGHT: u128 = 1 << 52;
    /// `StartUsingItem` input flag.
    pub const START_USING_ITEM: u128 = 1 << 53;
}

use crate::player::{Controls, PlayerState};

/// Tracks edge-triggered flags (start/stop sprinting, sneaking, ...).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputTracker {
    prev_sprinting: bool,
    prev_sneaking: bool,
    prev_jump: bool,
    prev_swimming: bool,
}

/// Per-tick input summary for `PlayerAuthInput`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InputSnapshot {
    /// Input flags (see [`flags`]).
    pub flags: u128,
    /// `(x, z)` move vector (x: left positive), scaled like the client.
    pub move_vector: [f32; 2],
    /// Unscaled `(x, z)` movement input.
    pub raw_move_vector: [f32; 2],
}

impl InputTracker {
    /// Computes the flags for the tick that was just simulated.
    pub fn snapshot(&mut self, state: &PlayerState, c: &Controls) -> InputSnapshot {
        use flags::*;
        let mut f = 0u128;
        if c.forward {
            f |= UP;
        }
        if c.back {
            f |= DOWN;
        }
        if c.left {
            f |= LEFT;
        }
        if c.right {
            f |= RIGHT;
        }
        if c.forward && c.left {
            f |= UP_LEFT;
        }
        if c.forward && c.right {
            f |= UP_RIGHT;
        }
        if c.back && c.left {
            f |= DOWN_LEFT;
        }
        if c.back && c.right {
            f |= DOWN_RIGHT;
        }
        if c.jump {
            f |= JUMP_DOWN | JUMPING | WANT_UP;
            if !self.prev_jump {
                f |= START_JUMPING;
            }
            if state.in_water {
                f |= AUTO_JUMPING_IN_WATER;
            }
        }
        if c.sprint {
            f |= SPRINT_DOWN;
        }
        if state.sprinting {
            f |= SPRINTING;
        }
        if state.sprinting && !self.prev_sprinting {
            f |= START_SPRINTING;
        }
        if !state.sprinting && self.prev_sprinting {
            f |= STOP_SPRINTING;
        }
        if c.sneak {
            f |= SNEAK_DOWN | SNEAKING | WANT_DOWN;
        }
        if state.sneaking && !self.prev_sneaking {
            f |= START_SNEAKING;
        }
        if !state.sneaking && self.prev_sneaking {
            f |= STOP_SNEAKING;
        }
        let swimming = state.in_water && state.sprinting;
        if swimming && !self.prev_swimming {
            f |= START_SWIMMING;
        }
        if !swimming && self.prev_swimming {
            f |= STOP_SWIMMING;
        }
        if state.horizontal_collision {
            f |= HORIZONTAL_COLLISION;
        }
        if state.vertical_collision {
            f |= VERTICAL_COLLISION;
        }
        self.prev_sprinting = state.sprinting;
        self.prev_sneaking = state.sneaking;
        self.prev_jump = c.jump;
        self.prev_swimming = swimming;

        let (s, fw) = c.move_vector();
        let scale = if c.sneak { 0.3 } else { 1.0 };
        let len = (s * s + fw * fw).sqrt().max(1.0);
        InputSnapshot {
            flags: f,
            move_vector: [(s / len * scale) as f32, (fw / len * scale) as f32],
            raw_move_vector: [s as f32, fw as f32],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Vec3;

    #[test]
    fn edge_flags_fire_once() {
        let mut t = InputTracker::default();
        let mut s = PlayerState::new(Vec3::ZERO);
        let c = Controls {
            forward: true,
            sprint: true,
            ..Default::default()
        };
        s.sprinting = true;
        let a = t.snapshot(&s, &c);
        assert_ne!(a.flags & flags::START_SPRINTING, 0);
        assert_ne!(a.flags & flags::UP, 0);
        let b = t.snapshot(&s, &c);
        assert_eq!(b.flags & flags::START_SPRINTING, 0);
        s.sprinting = false;
        let c2 = t.snapshot(&s, &Controls::default());
        assert_ne!(c2.flags & flags::STOP_SPRINTING, 0);
        assert_eq!(c2.move_vector, [0.0, 0.0]);
    }
}
