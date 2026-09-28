//! Player movement simulation.
//!
//! The model follows the vanilla movement rules that Bedrock's
//! server-authoritative movement validates against: gravity 0.08 blocks/tick²
//! with 0.98 vertical drag, ground inertia `slipperiness × 0.91`, 0.6 block
//! step-up, jump impulse 0.42, sprint-jump boost 0.2 and fluid drag. Small
//! residual differences are expected and are absorbed by the server's
//! correction tolerance; [`PlayerState::apply_correction`] handles
//! `CorrectPlayerMovePrediction` / teleport packets.

use torchflower_world::{BlockFlags, BlockPos, Shape, SparseWorld};

use crate::math::{Aabb, Vec3};

/// Read-only world access needed by the physics simulation.
pub trait CollisionWorld {
    /// Collision shape; unloaded blocks must report [`Shape::FULL`].
    fn shape(&self, pos: BlockPos) -> Shape;
    /// Material flags; unloaded blocks report [`BlockFlags::NONE`].
    fn flags(&self, pos: BlockPos) -> BlockFlags;
    /// Surface friction (0.6 default, 0.98 ice).
    fn friction(&self, pos: BlockPos) -> f32;
}

impl CollisionWorld for SparseWorld {
    fn shape(&self, pos: BlockPos) -> Shape {
        self.shape_at(pos).unwrap_or(Shape::FULL)
    }
    fn flags(&self, pos: BlockPos) -> BlockFlags {
        self.flags_at(pos).unwrap_or(BlockFlags::NONE)
    }
    fn friction(&self, pos: BlockPos) -> f32 {
        self.block_at(pos).map(|b| b.friction()).unwrap_or(0.6)
    }
}

/// Movement constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicsConfig {
    pub gravity: f64,
    pub air_drag: f64,
    pub air_inertia: f64,
    pub default_slipperiness: f64,
    pub step_height: f64,
    pub width: f64,
    pub height: f64,
    pub sneak_height: f64,
    pub eye_height: f64,
    pub jump_velocity: f64,
    pub sprint_jump_boost: f64,
    pub walk_speed: f64,
    pub sprint_multiplier: f64,
    pub sneak_multiplier: f64,
    pub air_acceleration: f64,
    pub sprint_air_acceleration: f64,
    pub water_drag: f64,
    pub water_gravity: f64,
    pub water_acceleration: f64,
    pub lava_drag: f64,
    pub climb_speed: f64,
    pub jump_cooldown_ticks: u8,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            gravity: 0.08,
            air_drag: 0.98,
            air_inertia: 0.91,
            default_slipperiness: 0.6,
            step_height: 0.6,
            width: 0.6,
            height: 1.8,
            sneak_height: 1.5,
            eye_height: 1.62,
            jump_velocity: 0.42,
            sprint_jump_boost: 0.2,
            walk_speed: 0.1,
            sprint_multiplier: 1.3,
            sneak_multiplier: 0.3,
            air_acceleration: 0.02,
            sprint_air_acceleration: 0.026,
            water_drag: 0.8,
            water_gravity: 0.02,
            water_acceleration: 0.02,
            lava_drag: 0.5,
            climb_speed: 0.2,
            jump_cooldown_ticks: 10,
        }
    }
}

/// Requested inputs for one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Controls {
    pub forward: bool,
    pub back: bool,
    pub left: bool,
    pub right: bool,
    pub jump: bool,
    pub sprint: bool,
    pub sneak: bool,
}

impl Controls {
    /// `(strafe, forward)` in −1..=1 (strafe: left positive).
    pub fn move_vector(&self) -> (f64, f64) {
        let f = self.forward as i8 as f64 - self.back as i8 as f64;
        let s = self.left as i8 as f64 - self.right as i8 as f64;
        (s, f)
    }
}

/// Active status effects relevant for movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MovementEffects {
    pub speed: u8,
    pub slowness: u8,
    pub jump_boost: u8,
    pub slow_falling: bool,
    pub levitation: u8,
}

/// Full per-bot movement state (≈150 bytes, no heap).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerState {
    /// Feet position.
    pub pos: Vec3,
    pub vel: Vec3,
    /// Degrees, Bedrock convention (0 = +Z/south, 90 = −X/west).
    pub yaw: f32,
    /// Degrees, positive looks down.
    pub pitch: f32,
    pub on_ground: bool,
    pub in_water: bool,
    pub in_lava: bool,
    pub on_climbable: bool,
    pub in_cobweb: bool,
    pub sprinting: bool,
    pub sneaking: bool,
    pub horizontal_collision: bool,
    pub vertical_collision: bool,
    pub fall_distance: f64,
    pub jump_cooldown: u8,
    pub effects: MovementEffects,
    /// Movement speed attribute (0.1 base), updated from `UpdateAttributes`.
    pub movement_speed: f64,
    /// Set when the latest tick started a jump.
    pub jumped_this_tick: bool,
}

impl PlayerState {
    /// New state at `feet`.
    pub fn new(feet: Vec3) -> Self {
        Self {
            pos: feet,
            vel: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
            in_water: false,
            in_lava: false,
            on_climbable: false,
            in_cobweb: false,
            sprinting: false,
            sneaking: false,
            horizontal_collision: false,
            vertical_collision: false,
            fall_distance: 0.0,
            jump_cooldown: 0,
            effects: MovementEffects::default(),
            movement_speed: 0.1,
            jumped_this_tick: false,
        }
    }

    /// Hitbox at the current position.
    pub fn aabb(&self, cfg: &PhysicsConfig) -> Aabb {
        let h = if self.sneaking {
            cfg.sneak_height
        } else {
            cfg.height
        };
        Aabb::from_feet(self.pos, cfg.width, h)
    }

    /// Eye position (what Bedrock sends as the player position).
    pub fn eye(&self, cfg: &PhysicsConfig) -> Vec3 {
        self.pos + Vec3::new(0.0, cfg.eye_height, 0.0)
    }

    /// Applies a server position correction / teleport.
    pub fn apply_correction(&mut self, feet: Vec3, velocity: Option<Vec3>, on_ground: bool) {
        self.pos = feet;
        self.vel = velocity.unwrap_or(Vec3::ZERO);
        self.on_ground = on_ground;
        self.fall_distance = 0.0;
    }

    /// Block the feet are standing in.
    pub fn block_pos(&self) -> BlockPos {
        BlockPos::from_f64(self.pos.x, self.pos.y, self.pos.z)
    }

    /// Unit look direction.
    pub fn look_dir(&self) -> Vec3 {
        let yaw = (self.yaw as f64).to_radians();
        let pitch = (self.pitch as f64).to_radians();
        Vec3::new(
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            yaw.cos() * pitch.cos(),
        )
    }

    /// Points the view at `target`.
    pub fn look_at(&mut self, cfg: &PhysicsConfig, target: Vec3) {
        let (yaw, pitch) = look_angles(self.eye(cfg), target);
        self.yaw = yaw;
        self.pitch = pitch;
    }
}

/// `(yaw, pitch)` in degrees looking from `from` to `to`.
pub fn look_angles(from: Vec3, to: Vec3) -> (f32, f32) {
    let d = to - from;
    let yaw = (-d.x).atan2(d.z).to_degrees();
    let horiz = d.horizontal_length();
    let pitch = (-d.y).atan2(horiz).to_degrees();
    (yaw as f32, pitch as f32)
}

/// Events produced by one physics tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TickOutcome {
    /// Fall damage (half-hearts) predicted on landing.
    pub fall_damage: f32,
    /// Position delta applied this tick.
    pub delta: Vec3,
    pub landed: bool,
}

/// Reusable simulator (owns a scratch collision buffer; no per-tick allocation).
#[derive(Debug, Clone, Default)]
pub struct Physics {
    pub cfg: PhysicsConfig,
    scratch: Vec<Aabb>,
}

impl Physics {
    /// Creates a simulator.
    pub fn new(cfg: PhysicsConfig) -> Self {
        Self {
            cfg,
            scratch: Vec::with_capacity(64),
        }
    }

    fn collect_boxes(&mut self, world: &impl CollisionWorld, region: Aabb) {
        self.scratch.clear();
        let x0 = region.min.x.floor() as i32;
        let x1 = region.max.x.floor() as i32;
        // Fences extend 0.5 above their cell.
        let y0 = region.min.y.floor() as i32 - 1;
        let y1 = region.max.y.floor() as i32;
        let z0 = region.min.z.floor() as i32;
        let z1 = region.max.z.floor() as i32;
        for x in x0..=x1 {
            for y in y0..=y1 {
                for z in z0..=z1 {
                    let pos = BlockPos::new(x, y, z);
                    let (boxes, n) = world.shape(pos).boxes();
                    for b in &boxes[..n] {
                        let bb = Aabb::new(
                            Vec3::new(
                                x as f64 + b[0] as f64,
                                y as f64 + b[1] as f64,
                                z as f64 + b[2] as f64,
                            ),
                            Vec3::new(
                                x as f64 + b[3] as f64,
                                y as f64 + b[4] as f64,
                                z as f64 + b[5] as f64,
                            ),
                        );
                        if bb.max.y > region.min.y && bb.min.y < region.max.y {
                            self.scratch.push(bb);
                        }
                    }
                }
            }
        }
    }

    fn collide(&self, bb: Aabb, d: Vec3) -> Vec3 {
        let mut dy = d.y;
        for b in &self.scratch {
            dy = bb.clip_y(b, dy);
        }
        let bb = bb.offset(Vec3::new(0.0, dy, 0.0));
        let mut dx = d.x;
        for b in &self.scratch {
            dx = bb.clip_x(b, dx);
        }
        let bb = bb.offset(Vec3::new(dx, 0.0, 0.0));
        let mut dz = d.z;
        for b in &self.scratch {
            dz = bb.clip_z(b, dz);
        }
        Vec3::new(dx, dy, dz)
    }

    fn has_collision(&mut self, world: &impl CollisionWorld, bb: Aabb) -> bool {
        self.collect_boxes(world, bb);
        self.scratch.iter().any(|b| b.intersects(&bb))
    }

    /// Moves the player by `d` with collision resolution, step-up and sneak
    /// edge protection. Returns the applied movement.
    pub fn move_player(
        &mut self,
        s: &mut PlayerState,
        world: &impl CollisionWorld,
        mut d: Vec3,
    ) -> Vec3 {
        let cfg = self.cfg;
        if s.in_cobweb {
            d = Vec3::new(d.x * 0.25, d.y * 0.05, d.z * 0.25);
            s.vel = Vec3::ZERO;
        }
        let bb = s.aabb(&cfg);

        // Sneak edge protection: never walk off a ledge while sneaking.
        if s.sneaking && s.on_ground && d.y <= 0.0 {
            let step = 0.05;
            let probe = |p: &mut Physics, dx: f64, dz: f64| {
                !p.has_collision(world, bb.offset(Vec3::new(dx, -cfg.step_height, dz)))
            };
            while d.x != 0.0 && probe(self, d.x, 0.0) {
                d.x = if d.x.abs() < step {
                    0.0
                } else {
                    d.x - step * d.x.signum()
                };
            }
            while d.z != 0.0 && probe(self, 0.0, d.z) {
                d.z = if d.z.abs() < step {
                    0.0
                } else {
                    d.z - step * d.z.signum()
                };
            }
            while d.x != 0.0 && d.z != 0.0 && probe(self, d.x, d.z) {
                d.x = if d.x.abs() < step {
                    0.0
                } else {
                    d.x - step * d.x.signum()
                };
                d.z = if d.z.abs() < step {
                    0.0
                } else {
                    d.z - step * d.z.signum()
                };
            }
        }

        self.collect_boxes(
            world,
            bb.expand_towards(d)
                .expand_towards(Vec3::new(0.0, cfg.step_height, 0.0)),
        );
        let mut moved = self.collide(bb, d);

        let blocked_h = moved.x != d.x || moved.z != d.z;
        let falling_onto = d.y < 0.0 && moved.y != d.y;
        if cfg.step_height > 0.0 && blocked_h && (s.on_ground || falling_onto) {
            // Try stepping up.
            let up = self.collide(bb, Vec3::new(d.x, cfg.step_height, d.z));
            let raised = {
                let only_up = self.collide(
                    bb.expand_towards(Vec3::new(d.x, 0.0, d.z)),
                    Vec3::new(0.0, cfg.step_height, 0.0),
                );
                if only_up.y < cfg.step_height {
                    let horiz = self.collide(
                        bb.offset(Vec3::new(0.0, only_up.y, 0.0)),
                        Vec3::new(d.x, 0.0, d.z),
                    );
                    Vec3::new(horiz.x, only_up.y, horiz.z)
                } else {
                    up
                }
            };
            let lifted = bb.offset(raised);
            let down = self.collide(lifted, Vec3::new(0.0, -raised.y + d.y.min(0.0), 0.0));
            let stepped = Vec3::new(raised.x, raised.y + down.y, raised.z);
            if stepped.horizontal_length() > moved.horizontal_length() + 1e-7 {
                moved = stepped;
            }
        }

        s.pos = s.pos + moved;
        s.horizontal_collision = (moved.x - d.x).abs() > 1e-7 || (moved.z - d.z).abs() > 1e-7;
        s.vertical_collision = (moved.y - d.y).abs() > 1e-7;
        s.on_ground = s.vertical_collision && d.y < 0.0;
        if (moved.x - d.x).abs() > 1e-7 {
            s.vel.x = 0.0;
        }
        if (moved.z - d.z).abs() > 1e-7 {
            s.vel.z = 0.0;
        }
        if s.vertical_collision {
            s.vel.y = 0.0;
        }
        moved
    }

    fn update_fluids(&mut self, s: &mut PlayerState, world: &impl CollisionWorld) {
        let bb = s.aabb(&self.cfg).deflate(0.001);
        let (mut water, mut lava, mut climb, mut web) = (false, false, false, false);
        let x0 = bb.min.x.floor() as i32;
        let x1 = bb.max.x.floor() as i32;
        let y0 = bb.min.y.floor() as i32;
        let y1 = bb.max.y.floor() as i32;
        let z0 = bb.min.z.floor() as i32;
        let z1 = bb.max.z.floor() as i32;
        for x in x0..=x1 {
            for y in y0..=y1 {
                for z in z0..=z1 {
                    let f = world.flags(BlockPos::new(x, y, z));
                    water |= f.contains(BlockFlags::WATER);
                    lava |= f.contains(BlockFlags::LAVA);
                    web |= f.contains(BlockFlags::SLOW)
                        && matches!(world.shape(BlockPos::new(x, y, z)), Shape::Empty);
                }
            }
        }
        let feet = s.block_pos();
        climb |= world.flags(feet).contains(BlockFlags::CLIMBABLE);
        s.in_water = water;
        s.in_lava = lava;
        s.on_climbable = climb;
        s.in_cobweb = web;
    }

    /// Advances the simulation by one 50 ms tick.
    pub fn tick(
        &mut self,
        s: &mut PlayerState,
        controls: Controls,
        world: &impl CollisionWorld,
    ) -> TickOutcome {
        let cfg = self.cfg;
        self.update_fluids(s, world);
        s.jumped_this_tick = false;
        if s.jump_cooldown > 0 {
            s.jump_cooldown -= 1;
        }

        let (mut strafe, mut forward) = controls.move_vector();
        s.sneaking = controls.sneak;
        s.sprinting =
            controls.sprint && forward > 0.0 && !controls.sneak && !s.horizontal_collision;
        if s.sneaking {
            strafe *= cfg.sneak_multiplier;
            forward *= cfg.sneak_multiplier;
        }
        strafe *= 0.98;
        forward *= 0.98;

        // Jumping.
        if controls.jump {
            if s.in_water || s.in_lava {
                s.vel.y += 0.04;
            } else if s.on_climbable {
                // Handled below.
            } else if s.on_ground && s.jump_cooldown == 0 {
                s.vel.y = cfg.jump_velocity + 0.1 * s.effects.jump_boost as f64;
                if s.sprinting {
                    let yaw = (s.yaw as f64).to_radians();
                    s.vel.x -= yaw.sin() * cfg.sprint_jump_boost;
                    s.vel.z += yaw.cos() * cfg.sprint_jump_boost;
                }
                s.jump_cooldown = cfg.jump_cooldown_ticks;
                s.jumped_this_tick = true;
            }
        } else {
            s.jump_cooldown = 0;
        }

        let start_y = s.pos.y;
        let was_on_ground = s.on_ground;
        let moved;
        if s.in_water || s.in_lava {
            let (drag, accel) = if s.in_water {
                (
                    if s.sprinting { 0.9 } else { cfg.water_drag },
                    cfg.water_acceleration,
                )
            } else {
                (cfg.lava_drag, cfg.water_acceleration)
            };
            apply_input(s, strafe, forward, accel);
            let v = s.vel;
            moved = self.move_player(s, world, v);
            s.vel = Vec3::new(s.vel.x * drag, s.vel.y * drag, s.vel.z * drag);
            s.vel.y -= cfg.water_gravity;
            if s.horizontal_collision {
                // Allow climbing out of water onto a ledge.
                s.vel.y = s.vel.y.max(0.3);
            }
        } else {
            let below = BlockPos::from_f64(s.pos.x, s.pos.y - 0.5, s.pos.z);
            let slip = if s.on_ground {
                world.friction(below) as f64
            } else {
                1.0
            };
            let inertia = if s.on_ground {
                slip * cfg.air_inertia
            } else {
                cfg.air_inertia
            };
            let mut speed = s.movement_speed;
            if s.sprinting {
                speed *= cfg.sprint_multiplier;
            }
            speed *= 1.0 + 0.2 * s.effects.speed as f64;
            speed *= (1.0 - 0.15 * s.effects.slowness as f64).max(0.0);
            let accel = if s.on_ground {
                speed * (0.216_000_02 / (slip * slip * slip))
            } else if s.sprinting {
                cfg.sprint_air_acceleration
            } else {
                cfg.air_acceleration
            };
            apply_input(s, strafe, forward, accel);
            if s.on_climbable {
                s.vel.x = s.vel.x.clamp(-0.15, 0.15);
                s.vel.z = s.vel.z.clamp(-0.15, 0.15);
                s.vel.y = s.vel.y.max(-0.15);
                if s.sneaking && s.vel.y < 0.0 {
                    s.vel.y = 0.0;
                }
            }
            let v = s.vel;
            moved = self.move_player(s, world, v);
            if s.on_climbable && (s.horizontal_collision || controls.jump) {
                s.vel.y = cfg.climb_speed;
            }
            if s.effects.levitation > 0 {
                s.vel.y += (0.05 * s.effects.levitation as f64 - s.vel.y) * 0.2;
            } else if s.effects.slow_falling && s.vel.y < 0.0 {
                s.vel.y -= 0.01;
            } else {
                s.vel.y -= cfg.gravity;
            }
            s.vel.y *= cfg.air_drag;
            s.vel.x *= inertia;
            s.vel.z *= inertia;
        }

        // Fall distance and damage.
        let mut out = TickOutcome {
            delta: moved,
            ..TickOutcome::default()
        };
        let dy = s.pos.y - start_y;
        if s.in_water || s.on_climbable || s.in_cobweb || s.effects.slow_falling {
            s.fall_distance = 0.0;
        } else if s.on_ground {
            if !was_on_ground {
                out.landed = true;
                let landing = world.flags(BlockPos::from_f64(s.pos.x, s.pos.y - 0.2, s.pos.z));
                let name_mult =
                    fall_multiplier(world, BlockPos::from_f64(s.pos.x, s.pos.y - 0.2, s.pos.z));
                let raw = (s.fall_distance - 3.0 - s.effects.jump_boost as f64)
                    .ceil()
                    .max(0.0);
                out.fall_damage = if landing.contains(BlockFlags::LIQUID) {
                    0.0
                } else {
                    (raw * name_mult) as f32
                };
            }
            s.fall_distance = 0.0;
        } else if dy < 0.0 {
            s.fall_distance -= dy;
        }
        out
    }
}

fn fall_multiplier(world: &impl CollisionWorld, pos: BlockPos) -> f64 {
    // Honey/hay reduce damage; approximated via friction/slow flags.
    let flags = world.flags(pos);
    if flags.contains(BlockFlags::SLOW) {
        0.2
    } else {
        1.0
    }
}

fn apply_input(s: &mut PlayerState, strafe: f64, forward: f64, accel: f64) {
    let mut len = strafe * strafe + forward * forward;
    if len < 1e-4 {
        return;
    }
    len = len.sqrt().max(1.0);
    let (strafe, forward) = (strafe / len * accel, forward / len * accel);
    let yaw = (s.yaw as f64).to_radians();
    let (sin, cos) = yaw.sin_cos();
    s.vel.x += strafe * cos - forward * sin;
    s.vel.z += forward * cos + strafe * sin;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flat stone floor at y < 64, plus custom blocks.
    struct TestWorld {
        extra: Vec<(BlockPos, Shape, BlockFlags, f32)>,
        floor_y: i32,
    }

    impl TestWorld {
        fn flat() -> Self {
            Self {
                extra: Vec::new(),
                floor_y: 63,
            }
        }
        fn find(&self, pos: BlockPos) -> Option<&(BlockPos, Shape, BlockFlags, f32)> {
            self.extra.iter().find(|e| e.0 == pos)
        }
    }

    impl CollisionWorld for TestWorld {
        fn shape(&self, pos: BlockPos) -> Shape {
            if let Some(e) = self.find(pos) {
                return e.1;
            }
            if pos.y <= self.floor_y {
                Shape::FULL
            } else {
                Shape::Empty
            }
        }
        fn flags(&self, pos: BlockPos) -> BlockFlags {
            self.find(pos).map(|e| e.2).unwrap_or(BlockFlags::NONE)
        }
        fn friction(&self, pos: BlockPos) -> f32 {
            self.find(pos).map(|e| e.3).unwrap_or(0.6)
        }
    }

    fn settle(p: &mut Physics, s: &mut PlayerState, w: &TestWorld) {
        for _ in 0..40 {
            p.tick(s, Controls::default(), w);
        }
    }

    #[test]
    fn falls_and_lands_on_floor() {
        let w = TestWorld::flat();
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 70.0, 0.5));
        settle(&mut p, &mut s, &w);
        assert!(s.on_ground);
        assert!((s.pos.y - 64.0).abs() < 1e-9);
    }

    #[test]
    fn free_fall_matches_vanilla_first_ticks() {
        let w = TestWorld {
            extra: vec![],
            floor_y: -1000,
        };
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 100.0, 0.5));
        p.tick(&mut s, Controls::default(), &w);
        // Tick 1 moves by 0, then velocity becomes -0.0784.
        assert!((s.vel.y + 0.0784).abs() < 1e-9);
        p.tick(&mut s, Controls::default(), &w);
        assert!((s.pos.y - (100.0 - 0.0784)).abs() < 1e-9);
    }

    #[test]
    fn walking_reaches_vanilla_speed() {
        let w = TestWorld::flat();
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 64.0, 0.5));
        settle(&mut p, &mut s, &w);
        let c = Controls {
            forward: true,
            ..Default::default()
        };
        let mut last = s.pos;
        let mut speed = 0.0;
        for _ in 0..60 {
            p.tick(&mut s, c, &w);
            speed = (s.pos - last).horizontal_length();
            last = s.pos;
        }
        // Vanilla walking ≈ 4.317 m/s ≈ 0.2159 blocks/tick.
        assert!(
            (speed * 20.0 - 4.317).abs() < 0.05,
            "speed {}",
            speed * 20.0
        );
        assert!(s.pos.z > 5.0, "yaw 0 walks towards +Z");
    }

    #[test]
    fn sprinting_is_faster() {
        let w = TestWorld::flat();
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 64.0, 0.5));
        settle(&mut p, &mut s, &w);
        let c = Controls {
            forward: true,
            sprint: true,
            ..Default::default()
        };
        let mut last = s.pos;
        let mut speed = 0.0;
        for _ in 0..60 {
            p.tick(&mut s, c, &w);
            speed = (s.pos - last).horizontal_length();
            last = s.pos;
        }
        assert!(
            (speed * 20.0 - 5.612).abs() < 0.08,
            "speed {}",
            speed * 20.0
        );
    }

    #[test]
    fn jump_reaches_about_1_25_blocks() {
        let w = TestWorld::flat();
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 64.0, 0.5));
        settle(&mut p, &mut s, &w);
        let mut peak: f64 = 0.0;
        p.tick(
            &mut s,
            Controls {
                jump: true,
                ..Default::default()
            },
            &w,
        );
        for _ in 0..30 {
            p.tick(&mut s, Controls::default(), &w);
            peak = peak.max(s.pos.y - 64.0);
        }
        assert!((peak - 1.2522).abs() < 0.01, "peak {peak}");
        assert!(s.on_ground);
    }

    #[test]
    fn steps_onto_slab_but_not_full_block() {
        let mut w = TestWorld::flat();
        w.extra.push((
            BlockPos::new(0, 64, 2),
            Shape::Box { lo: 0, hi: 8 },
            BlockFlags::SOLID,
            0.6,
        ));
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 64.0, 0.5));
        settle(&mut p, &mut s, &w);
        let c = Controls {
            forward: true,
            ..Default::default()
        };
        for _ in 0..10 {
            p.tick(&mut s, c, &w);
        }
        assert!((s.pos.y - 64.5).abs() < 1e-6, "y {}", s.pos.y);

        let mut w = TestWorld::flat();
        w.extra
            .push((BlockPos::new(0, 64, 2), Shape::FULL, BlockFlags::SOLID, 0.6));
        let mut s = PlayerState::new(Vec3::new(0.5, 64.0, 0.5));
        settle(&mut p, &mut s, &w);
        for _ in 0..20 {
            p.tick(&mut s, c, &w);
        }
        assert!((s.pos.y - 64.0).abs() < 1e-6);
        assert!(s.pos.z <= 1.7 + 1e-9);
        assert!(s.horizontal_collision);
    }

    #[test]
    fn sneaking_does_not_walk_off_edges() {
        let mut w = TestWorld {
            extra: vec![],
            floor_y: 40,
        };
        for z in 0..2 {
            w.extra
                .push((BlockPos::new(0, 63, z), Shape::FULL, BlockFlags::SOLID, 0.6));
        }
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 64.0, 0.5));
        settle(&mut p, &mut s, &w);
        let c = Controls {
            forward: true,
            sneak: true,
            ..Default::default()
        };
        for _ in 0..80 {
            p.tick(&mut s, c, &w);
        }
        assert!(s.on_ground);
        assert!((s.pos.y - 64.0).abs() < 1e-9);
        assert!(s.pos.z < 2.3 + 1e-9);
    }

    #[test]
    fn fall_damage_after_long_drop() {
        let w = TestWorld::flat();
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 74.0, 0.5));
        let mut dmg = 0.0;
        for _ in 0..80 {
            let o = p.tick(&mut s, Controls::default(), &w);
            dmg += o.fall_damage;
        }
        assert_eq!(dmg, 7.0);
    }

    #[test]
    fn ice_is_slippery() {
        let mut w = TestWorld::flat();
        for z in -2..40 {
            w.extra.push((
                BlockPos::new(0, 63, z),
                Shape::FULL,
                BlockFlags::SOLID,
                0.98,
            ));
        }
        let mut p = Physics::default();
        let mut s = PlayerState::new(Vec3::new(0.5, 64.0, 0.5));
        settle(&mut p, &mut s, &w);
        s.vel.z = 0.3;
        let start = s.pos.z;
        for _ in 0..10 {
            p.tick(&mut s, Controls::default(), &w);
        }
        assert!(s.pos.z - start > 1.5, "slid {}", s.pos.z - start);
    }

    #[test]
    fn look_angles_convention() {
        let (yaw, pitch) = look_angles(Vec3::ZERO, Vec3::new(0.0, 0.0, 1.0));
        assert!(yaw.abs() < 1e-4 && pitch.abs() < 1e-4);
        let (yaw, _) = look_angles(Vec3::ZERO, Vec3::new(-1.0, 0.0, 0.0));
        assert!((yaw - 90.0).abs() < 1e-4);
        let (_, pitch) = look_angles(Vec3::ZERO, Vec3::new(0.0, -1.0, 0.0));
        assert!((pitch - 90.0).abs() < 1e-4);
    }
}
