//! Navigation goals.

use torchflower_world::BlockPos;

use crate::search::WALK_COST;

/// A navigation goal over feet positions.
pub trait Goal {
    /// Estimated remaining cost from `pos`.
    fn heuristic(&self, pos: BlockPos) -> f32;
    /// True if `pos` satisfies the goal.
    fn is_end(&self, pos: BlockPos) -> bool;
}

fn octile(dx: i32, dy: i32, dz: i32) -> f32 {
    let (a, b) = {
        let (x, z) = (dx.unsigned_abs() as f32, dz.unsigned_abs() as f32);
        (x.max(z), x.min(z))
    };
    let horiz = (a - b) + b * std::f32::consts::SQRT_2;
    horiz * WALK_COST + dy.unsigned_abs() as f32 * WALK_COST * 0.5
}

/// Stand exactly at a block position (feet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalBlock(pub BlockPos);

impl GoalBlock {
    pub fn new(x: i32, y: i32, z: i32) -> Self {
        Self(BlockPos::new(x, y, z))
    }
}

impl Goal for GoalBlock {
    fn heuristic(&self, p: BlockPos) -> f32 {
        octile(self.0.x - p.x, self.0.y - p.y, self.0.z - p.z)
    }
    fn is_end(&self, p: BlockPos) -> bool {
        p == self.0
    }
}

/// Get within `range` blocks (Euclidean, feet) of a position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GoalNear {
    pub pos: BlockPos,
    pub range: f32,
}

impl GoalNear {
    pub fn new(pos: BlockPos, range: impl Into<f64>) -> Self {
        Self {
            pos,
            range: range.into() as f32,
        }
    }
}

impl Goal for GoalNear {
    fn heuristic(&self, p: BlockPos) -> f32 {
        let d = octile(self.pos.x - p.x, self.pos.y - p.y, self.pos.z - p.z);
        (d - self.range * WALK_COST).max(0.0)
    }
    fn is_end(&self, p: BlockPos) -> bool {
        (p.dist_sq(self.pos) as f32) <= self.range * self.range
    }
}

/// Reach a column (any Y).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalXZ {
    pub x: i32,
    pub z: i32,
}

impl Goal for GoalXZ {
    fn heuristic(&self, p: BlockPos) -> f32 {
        octile(self.x - p.x, 0, self.z - p.z)
    }
    fn is_end(&self, p: BlockPos) -> bool {
        p.x == self.x && p.z == self.z
    }
}

/// Reach a Y level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalY(pub i32);

impl Goal for GoalY {
    fn heuristic(&self, p: BlockPos) -> f32 {
        (self.0 - p.y).unsigned_abs() as f32 * WALK_COST
    }
    fn is_end(&self, p: BlockPos) -> bool {
        p.y == self.0
    }
}

/// Stand next to (touching) a block, e.g. to dig it or open a chest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalGetToBlock(pub BlockPos);

impl Goal for GoalGetToBlock {
    fn heuristic(&self, p: BlockPos) -> f32 {
        let d = octile(self.0.x - p.x, self.0.y - p.y, self.0.z - p.z);
        (d - WALK_COST).max(0.0)
    }
    fn is_end(&self, p: BlockPos) -> bool {
        let dx = (p.x - self.0.x).abs();
        let dz = (p.z - self.0.z).abs();
        let dy = self.0.y - p.y;
        dx + dz <= 1 && (-1..=1).contains(&dy) && !(dx == 0 && dz == 0 && dy == 0)
    }
}

impl<G: Goal + ?Sized> Goal for &G {
    fn heuristic(&self, p: BlockPos) -> f32 {
        (**self).heuristic(p)
    }
    fn is_end(&self, p: BlockPos) -> bool {
        (**self).is_end(p)
    }
}

impl<G: Goal + ?Sized> Goal for Box<G> {
    fn heuristic(&self, p: BlockPos) -> f32 {
        (**self).heuristic(p)
    }
    fn is_end(&self, p: BlockPos) -> bool {
        (**self).is_end(p)
    }
}
