//! Voxel raycasting (Amanatides–Woo DDA with per-box refinement).

use crate::block::Shape;
use crate::world::{BlockPos, SparseWorld};

/// Result of a successful raycast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RaycastHit {
    /// Block that was hit.
    pub pos: BlockPos,
    /// Bedrock face id that was hit (0 down … 5 east).
    pub face: u8,
    /// Exact hit point in world coordinates.
    pub point: [f64; 3],
    /// Distance from the origin.
    pub distance: f64,
}

fn ray_box(origin: [f64; 3], dir: [f64; 3], min: [f64; 3], max: [f64; 3]) -> Option<(f64, u8)> {
    let mut t_near = f64::NEG_INFINITY;
    let mut t_far = f64::INFINITY;
    let mut face = 0u8;
    for axis in 0..3 {
        if dir[axis].abs() < 1e-12 {
            if origin[axis] < min[axis] || origin[axis] > max[axis] {
                return None;
            }
            continue;
        }
        let inv = 1.0 / dir[axis];
        let mut t0 = (min[axis] - origin[axis]) * inv;
        let mut t1 = (max[axis] - origin[axis]) * inv;
        // Face entered when travelling in +axis is the "min" face.
        let entering_face = match (axis, dir[axis] > 0.0) {
            (0, true) => 4,
            (0, false) => 5,
            (1, true) => 0,
            (1, false) => 1,
            (_, true) => 2,
            (_, false) => 3,
        };
        if t0 > t1 {
            std::mem::swap(&mut t0, &mut t1);
        }
        if t0 > t_near {
            t_near = t0;
            face = entering_face;
        }
        t_far = t_far.min(t1);
        if t_near > t_far {
            return None;
        }
    }
    (t_far >= 0.0).then_some((t_near.max(0.0), face))
}

impl SparseWorld {
    /// Casts a ray and returns the first block with collision (unloaded data
    /// counts as a full block). `dir` need not be normalised.
    pub fn raycast(
        &self,
        origin: [f64; 3],
        dir: [f64; 3],
        max_distance: f64,
    ) -> Option<RaycastHit> {
        self.raycast_with(origin, dir, max_distance, |world, pos| {
            world.shape_at(pos).unwrap_or(Shape::FULL)
        })
    }

    /// Like [`SparseWorld::raycast`] but with a custom shape provider (e.g. to
    /// treat non-solid but targetable blocks such as flowers as full cubes).
    pub fn raycast_with(
        &self,
        origin: [f64; 3],
        dir: [f64; 3],
        max_distance: f64,
        shape_of: impl Fn(&SparseWorld, BlockPos) -> Shape,
    ) -> Option<RaycastHit> {
        let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
        if len < 1e-12 {
            return None;
        }
        let d = [dir[0] / len, dir[1] / len, dir[2] / len];
        let mut cell = [
            origin[0].floor() as i32,
            origin[1].floor() as i32,
            origin[2].floor() as i32,
        ];
        let step = [
            d[0].signum() as i32,
            d[1].signum() as i32,
            d[2].signum() as i32,
        ];
        let mut t_max = [0.0f64; 3];
        let mut t_delta = [f64::INFINITY; 3];
        for a in 0..3 {
            if d[a].abs() > 1e-12 {
                let next = if d[a] > 0.0 {
                    cell[a] as f64 + 1.0
                } else {
                    cell[a] as f64
                };
                t_max[a] = (next - origin[a]) / d[a];
                t_delta[a] = 1.0 / d[a].abs();
            } else {
                t_max[a] = f64::INFINITY;
            }
        }
        let max_steps = (max_distance * 3.0).ceil() as usize + 3;
        for _ in 0..max_steps {
            let pos = BlockPos::new(cell[0], cell[1], cell[2]);
            let shape = shape_of(self, pos);
            let (boxes, n) = shape.boxes();
            let mut best: Option<(f64, u8)> = None;
            for b in &boxes[..n] {
                let min = [
                    pos.x as f64 + b[0] as f64,
                    pos.y as f64 + b[1] as f64,
                    pos.z as f64 + b[2] as f64,
                ];
                let max = [
                    pos.x as f64 + b[3] as f64,
                    pos.y as f64 + b[4] as f64,
                    pos.z as f64 + b[5] as f64,
                ];
                if let Some(hit) = ray_box(origin, d, min, max) {
                    if best.is_none_or(|b| hit.0 < b.0) {
                        best = Some(hit);
                    }
                }
            }
            if let Some((t, face)) = best {
                if t <= max_distance {
                    return Some(RaycastHit {
                        pos,
                        face,
                        point: [
                            origin[0] + d[0] * t,
                            origin[1] + d[1] * t,
                            origin[2] + d[2] * t,
                        ],
                        distance: t,
                    });
                }
                return None;
            }
            let axis = if t_max[0] < t_max[1] {
                if t_max[0] < t_max[2] {
                    0
                } else {
                    2
                }
            } else if t_max[1] < t_max[2] {
                1
            } else {
                2
            };
            if t_max[axis] > max_distance {
                return None;
            }
            cell[axis] += step[axis];
            t_max[axis] += t_delta[axis];
        }
        None
    }

    /// True if `target` is the first solid block hit when looking from `eye`
    /// toward `point` (defaults to the block centre).
    pub fn can_see_block(&self, eye: [f64; 3], target: BlockPos, point: Option<[f64; 3]>) -> bool {
        let p = point.unwrap_or(target.center());
        let dir = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        let dist = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
        match self.raycast_with(eye, dir, dist + 0.5, |w, pos| {
            if pos == target {
                Shape::FULL
            } else {
                w.shape_at(pos).unwrap_or(Shape::FULL)
            }
        }) {
            Some(hit) => hit.pos == target,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{LevelChunk, SubChunk};
    use crate::palette::local_index;
    use crate::registry::{BlockRegistry, RuntimeIdMode};
    use crate::world::WindowConfig;
    use std::sync::Arc;
    use torchflower_protocol_core::wire::{put_var_i32, put_var_u32};

    #[test]
    fn ray_hits_floor_top_face() {
        let reg = Arc::new(BlockRegistry::from_states(
            vec![
                ("minecraft:air".to_string(), None),
                ("minecraft:stone".to_string(), None),
            ],
            RuntimeIdMode::Hashed,
        ));
        let air = reg.runtime_ids_for_name("air")[0];
        let stone = reg.runtime_ids_for_name("stone")[0];
        let mut sub = SubChunk::uniform(air);
        for x in 0..16 {
            for z in 0..16 {
                sub.layers[0]
                    .as_mut()
                    .unwrap()
                    .set(local_index(x, 0, z), stone);
            }
        }
        let mut body = Vec::new();
        sub.encode(4, &mut body);
        let mut pkt = Vec::new();
        for v in [0, 0, 0] {
            put_var_i32(&mut pkt, v);
        }
        put_var_u32(&mut pkt, 1);
        pkt.push(0);
        put_var_u32(&mut pkt, body.len() as u32);
        pkt.extend_from_slice(&body);
        let mut world = SparseWorld::new(reg, WindowConfig::default());
        world.set_center(BlockPos::new(8, 66, 8));
        world
            .insert_level_chunk(&LevelChunk::decode(&pkt).unwrap())
            .unwrap();
        let hit = world
            .raycast([8.5, 67.62, 8.5], [0.3, -1.0, 0.1], 6.0)
            .unwrap();
        assert_eq!(hit.pos.y, 64);
        assert_eq!(hit.face, 1);
        assert!((hit.point[1] - 65.0).abs() < 1e-9);
        assert!(world.can_see_block([8.5, 67.62, 8.5], BlockPos::new(8, 64, 8), None));
        assert!(!world.can_see_block([8.5, 67.62, 8.5], BlockPos::new(8, 63, 8), None));
    }
}
