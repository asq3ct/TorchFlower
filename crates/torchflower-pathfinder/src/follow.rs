//! Turns a planned path into per-tick controls for the physics controller.

use torchflower_physics::{look_angles, Controls, PlayerState, Vec3};
use torchflower_world::BlockPos;

use crate::search::{MoveKind, PathWorld, Step};

/// World interaction the follower needs the bot to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowAction {
    /// Break this block (then call `tick` again).
    Dig(BlockPos),
    /// Place a block at `pos`, clicking `against` on `face`.
    Place {
        /// Block position to fill.
        pos: BlockPos,
        /// Solid block to click.
        against: BlockPos,
        /// Face of `against` to click.
        face: u8,
    },
}

/// Follower state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowStatus {
    /// Following the path.
    Running,
    /// Every step has been reached.
    Done,
    /// No progress for too long; the caller should re-plan.
    Stuck,
}

/// Output for one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FollowOutput {
    /// Movement controls for this tick.
    pub controls: Controls,
    /// Desired `(yaw, pitch)`.
    pub look: Option<(f32, f32)>,
    /// Dig or place the bot must perform first.
    pub action: Option<FollowAction>,
    /// Follower status.
    pub status: FollowStatus,
}

/// Drives the bot along a sequence of [`Step`]s.
#[derive(Debug, Clone, Default)]
pub struct PathFollower {
    steps: Vec<Step>,
    index: usize,
    stall_ticks: u32,
    best_dist: f64,
    /// Ticks without progress before reporting [`FollowStatus::Stuck`].
    pub stuck_after: u32,
}

impl PathFollower {
    /// New follower for `steps`.
    pub fn new(steps: Vec<Step>) -> Self {
        Self {
            steps,
            index: 0,
            stall_ticks: 0,
            best_dist: f64::MAX,
            stuck_after: 60,
        }
    }

    /// Remaining steps.
    pub fn remaining(&self) -> &[Step] {
        &self.steps[self.index.min(self.steps.len())..]
    }

    /// True when every step has been reached.
    pub fn is_done(&self) -> bool {
        self.index >= self.steps.len()
    }

    /// Informs the follower that the server moved the bot (movement
    /// correction, rubber-band or teleport) to feet position `corrected`.
    ///
    /// Stall detection is reset so the jump does not count as lack of
    /// progress, and the follower re-anchors itself to the step nearest the
    /// corrected position — skipping ahead if the server pushed the bot
    /// forward, rewinding if it pulled the bot back down the path it already
    /// walked (the usual rubber-band). Returns `true` if the path can still be
    /// followed from there (some step is within `max_offset` blocks); `false`
    /// means the caller should re-plan.
    pub fn on_position_corrected(&mut self, corrected: Vec3, max_offset: f64) -> bool {
        self.stall_ticks = 0;
        self.best_dist = f64::MAX;
        if self.is_done() {
            return true;
        }
        // The whole path is searched, not just the remaining steps: a
        // rubber-band lands on a step the bot has already passed, and rewinding
        // to it re-walks a valid path instead of throwing it away.
        let mut nearest: Option<(usize, f64)> = None;
        for (i, step) in self.steps.iter().enumerate() {
            let c = Vec3::new(
                step.pos.x as f64 + 0.5,
                step.pos.y as f64,
                step.pos.z as f64 + 0.5,
            );
            let d = c.distance(corrected);
            if nearest.is_none_or(|(_, best)| d < best) {
                nearest = Some((i, d));
            }
        }
        let Some((i, d)) = nearest else {
            return true;
        };
        if i > self.index {
            // Only treat the bot as advanced when it really is on that step.
            if d < 0.5 {
                self.index = i;
            }
        } else if i < self.index && d <= max_offset {
            self.index = i;
        }
        d <= max_offset
    }

    /// Computes the controls for the next tick.
    pub fn tick(
        &mut self,
        s: &PlayerState,
        world: &impl PathWorld,
        eye_height: f64,
    ) -> FollowOutput {
        let idle = FollowOutput {
            controls: Controls::default(),
            look: None,
            action: None,
            status: FollowStatus::Done,
        };
        // Advance over reached steps.
        while let Some(step) = self.steps.get(self.index) {
            if reached(s, step) {
                self.index += 1;
                self.stall_ticks = 0;
                self.best_dist = f64::MAX;
            } else {
                break;
            }
        }
        let Some(step) = self.steps.get(self.index).copied() else {
            return idle;
        };
        let eye = s.pos + Vec3::new(0.0, eye_height, 0.0);

        // Pending digs.
        for d in step.dig.iter().flatten() {
            if world.shape(*d).is_some_and(|sh| !sh.is_empty()) {
                self.stall_ticks = 0;
                let c = d.center();
                return FollowOutput {
                    controls: Controls::default(),
                    look: Some(look_angles(eye, Vec3::new(c[0], c[1], c[2]))),
                    action: Some(FollowAction::Dig(*d)),
                    status: FollowStatus::Running,
                };
            }
        }

        let target = Vec3::new(
            step.pos.x as f64 + 0.5,
            step.pos.y as f64,
            step.pos.z as f64 + 0.5,
        );
        let delta = target - s.pos;
        let horiz = delta.horizontal_length();

        // Pending placements.
        if let Some(place) = step.place {
            let empty = world.shape(place).is_some_and(|sh| sh.is_empty());
            if empty {
                let against = place.offset(0, -1, 0);
                let ready = match step.kind {
                    // Pillar: place once the feet have risen above the block.
                    MoveKind::Pillar => s.pos.y >= place.y as f64 + 1.0 - 0.05,
                    _ => true,
                };
                let c = against.center();
                let look = look_angles(eye, Vec3::new(c[0], c[1] + 0.5, c[2]));
                let controls = Controls {
                    jump: step.kind == MoveKind::Pillar && s.on_ground,
                    sneak: step.kind == MoveKind::Bridge,
                    ..Controls::default()
                };
                return FollowOutput {
                    controls,
                    look: Some(look),
                    action: ready.then_some(FollowAction::Place {
                        pos: place,
                        against,
                        face: 1,
                    }),
                    status: FollowStatus::Running,
                };
            }
        }

        // Stall detection.
        let dist = delta.length();
        if dist + 0.05 < self.best_dist {
            self.best_dist = dist;
            self.stall_ticks = 0;
        } else {
            self.stall_ticks += 1;
            if self.stall_ticks > self.stuck_after {
                return FollowOutput {
                    status: FollowStatus::Stuck,
                    ..idle
                };
            }
        }

        let (yaw, _) = look_angles(s.pos, target);
        let mut controls = Controls {
            forward: horiz > 0.15,
            sprint: matches!(step.kind, MoveKind::Parkour) || (horiz > 1.5 && !s.in_water),
            ..Controls::default()
        };
        match step.kind {
            MoveKind::Ascend => {
                controls.jump =
                    s.on_ground && horiz < 1.3 && delta.y > 0.3 || s.horizontal_collision;
            }
            MoveKind::Parkour => {
                // Jump near the edge of the current block.
                let start = self
                    .index
                    .checked_sub(1)
                    .and_then(|i| self.steps.get(i))
                    .map(|p| p.pos);
                let from = start.map(|p| Vec3::new(p.x as f64 + 0.5, p.y as f64, p.z as f64 + 0.5));
                let moved = from.map(|f| (s.pos - f).horizontal_length()).unwrap_or(1.0);
                controls.jump = s.on_ground && moved > 0.35;
            }
            MoveKind::Swim | MoveKind::Pillar => {
                controls.jump = delta.y > -0.2;
            }
            _ => {
                controls.jump =
                    s.horizontal_collision && s.on_ground || (s.in_water && delta.y >= 0.0);
            }
        }
        if s.in_water && delta.y > 0.1 {
            controls.jump = true;
        }
        FollowOutput {
            controls,
            look: Some((yaw, 0.0)),
            action: None,
            status: FollowStatus::Running,
        }
    }
}

fn reached(s: &PlayerState, step: &Step) -> bool {
    let cx = step.pos.x as f64 + 0.5;
    let cz = step.pos.z as f64 + 0.5;
    let dx = s.pos.x - cx;
    let dz = s.pos.z - cz;
    let horiz_ok = dx * dx + dz * dz < 0.35 * 0.35;
    let y_ok = (s.pos.y - step.pos.y as f64).abs() < 0.6;
    horiz_ok && y_ok && (s.on_ground || s.in_water || s.on_climbable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::GoalBlock;
    use crate::search::{find_path, tests::Grid, PathOptions, PathStatus};
    use torchflower_physics::{CollisionWorld, Physics};
    use torchflower_world::{BlockFlags, Shape};

    struct Phys<'a>(&'a Grid);
    impl CollisionWorld for Phys<'_> {
        fn shape(&self, p: BlockPos) -> Shape {
            self.0.shape(p).unwrap_or(Shape::FULL)
        }
        fn flags(&self, p: BlockPos) -> BlockFlags {
            PathWorld::flags(self.0, p)
        }
        fn friction(&self, _p: BlockPos) -> f32 {
            0.6
        }
    }

    fn simulate(w: &Grid, start: BlockPos, goal: BlockPos, ticks: usize) -> PlayerState {
        let opt = PathOptions {
            allow_dig: false,
            ..Default::default()
        };
        let r = find_path(w, start, &GoalBlock(goal), &opt);
        assert_eq!(r.status, PathStatus::Complete);
        let mut f = PathFollower::new(r.steps);
        let mut physics = Physics::default();
        let mut s = PlayerState::new(Vec3::new(
            start.x as f64 + 0.5,
            start.y as f64,
            start.z as f64 + 0.5,
        ));
        s.on_ground = true;
        for _ in 0..ticks {
            let out = f.tick(&s, w, 1.62);
            if let Some((yaw, pitch)) = out.look {
                s.yaw = yaw;
                s.pitch = pitch;
            }
            assert!(out.action.is_none());
            if out.status == FollowStatus::Done {
                break;
            }
            assert_ne!(out.status, FollowStatus::Stuck, "stuck at {:?}", s.pos);
            physics.tick(&mut s, out.controls, &Phys(w));
        }
        s
    }

    #[test]
    fn follows_path_with_turns_and_steps() {
        let mut w = Grid::flat();
        for z in -3..=6 {
            w.set(BlockPos::new(4, 64, z), Shape::FULL);
        }
        w.set(BlockPos::new(2, 64, 0), Shape::FULL);
        w.set(BlockPos::new(2, 65, 0), Shape::FULL);
        let s = simulate(&w, BlockPos::new(0, 64, 0), BlockPos::new(8, 64, 3), 400);
        assert_eq!(s.block_pos(), BlockPos::new(8, 64, 3));
    }

    #[test]
    fn correction_keeps_path_and_resets_stall_detection() {
        let w = Grid::flat();
        let opt = PathOptions {
            allow_dig: false,
            ..Default::default()
        };
        let r = find_path(
            &w,
            BlockPos::new(0, 64, 0),
            &GoalBlock::new(10, 64, 0),
            &opt,
        );
        let mut f = PathFollower::new(r.steps);
        f.stall_ticks = 55;
        // Pulled back half a block: still on the path, nothing skipped.
        assert!(f.on_position_corrected(Vec3::new(0.3, 64.0, 0.5), 3.0));
        assert_eq!(f.stall_ticks, 0);
        assert_eq!(f.remaining().len(), 10);
        // Pushed forward onto step 5: the follower skips ahead.
        assert!(f.on_position_corrected(Vec3::new(5.5, 64.0, 0.5), 3.0));
        assert_eq!(f.remaining().first().unwrap().pos, BlockPos::new(5, 64, 0));
        // Rubber-banded back onto step 3, which the bot already walked: the
        // path is still valid, so the follower rewinds instead of giving up.
        assert!(f.on_position_corrected(Vec3::new(3.5, 64.0, 0.5), 3.0));
        assert_eq!(f.remaining().first().unwrap().pos, BlockPos::new(3, 64, 0));
        assert_eq!(f.remaining().len(), 8);
        // Teleported far away: caller must re-plan.
        assert!(!f.on_position_corrected(Vec3::new(40.5, 80.0, 40.5), 3.0));
    }

    /// A correction sideways off a path that turns must not silently re-anchor
    /// to a step the bot cannot walk to: it is too far, so a re-plan is asked
    /// for and the follower keeps its position in the path.
    #[test]
    fn correction_off_a_turning_path_asks_for_a_replan() {
        let w = Grid::flat();
        let opt = PathOptions {
            allow_dig: false,
            ..Default::default()
        };
        let r = find_path(&w, BlockPos::new(0, 64, 0), &GoalBlock::new(6, 64, 6), &opt);
        let mut f = PathFollower::new(r.steps);
        let before = f.remaining().len();
        assert!(before > 4, "path has some length: {before}");
        assert!(!f.on_position_corrected(Vec3::new(-20.5, 64.0, 20.5), 2.5));
        assert_eq!(f.remaining().len(), before, "index untouched on failure");
    }

    #[test]
    fn follows_parkour_jump() {
        let mut w = Grid::flat();
        for x in -5..=5 {
            for y in 40..=63 {
                w.set(BlockPos::new(x, y, 3), Shape::Empty);
                w.set(BlockPos::new(x, y, 4), Shape::Empty);
            }
        }
        let s = simulate(&w, BlockPos::new(0, 64, 0), BlockPos::new(0, 64, 7), 300);
        assert_eq!(s.block_pos(), BlockPos::new(0, 64, 7));
    }
}
