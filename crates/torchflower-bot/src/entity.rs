//! Compact, bounded entity table.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use torchflower_physics::{Aabb, Vec3};

/// Interns an entity identifier (bounded set of vanilla/custom types).
fn intern(kind: &str) -> &'static str {
    static SET: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let set = SET.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = match set.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(s) = guard.get(kind) {
        return s;
    }
    if guard.len() >= 4096 {
        return "minecraft:unknown";
    }
    let leaked: &'static str = Box::leak(kind.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// A tracked entity (~96 bytes; players additionally own their name).
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub runtime_id: u64,
    pub unique_id: i64,
    /// Identifier such as `minecraft:zombie` (interned).
    pub kind: &'static str,
    pub username: Option<Box<str>>,
    pub position: Vec3,
    pub velocity: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    /// For item entities: `(network_id, count)`.
    pub item: Option<(i32, u16)>,
    /// Network id of the held item (players/mobs).
    pub held_item: i32,
}

impl Entity {
    /// True for players.
    pub fn is_player(&self) -> bool {
        self.username.is_some()
    }

    /// Approximate hitbox.
    pub fn aabb(&self) -> Aabb {
        let (w, h) = entity_size(self.kind);
        Aabb::from_feet(self.position, w, h)
    }
}

/// `(width, height)` of common entity types; defaults to a player-sized box.
pub fn entity_size(kind: &str) -> (f64, f64) {
    match kind.strip_prefix("minecraft:").unwrap_or(kind) {
        "item" | "xp_orb" | "arrow" | "snowball" | "egg" => (0.25, 0.25),
        "chicken" | "parrot" => (0.4, 0.7),
        "pig" => (0.9, 0.9),
        "cow" | "mooshroom" => (0.9, 1.4),
        "sheep" => (0.9, 1.3),
        "spider" => (1.4, 0.9),
        "cave_spider" => (0.7, 0.5),
        "creeper" => (0.6, 1.7),
        "enderman" => (0.6, 2.9),
        "slime" | "magma_cube" => (1.04, 1.04),
        "horse" | "donkey" | "mule" => (1.4, 1.6),
        "wolf" => (0.6, 0.85),
        "cat" | "ocelot" => (0.6, 0.7),
        "villager" | "villager_v2" | "wandering_trader" => (0.6, 1.95),
        "iron_golem" => (1.4, 2.9),
        _ => (0.6, 1.8),
    }
}

/// Bounded entity table. Entities further than `track_radius` from the bot
/// are evicted; when full, the farthest entity is replaced.
#[derive(Debug, Clone)]
pub struct EntityTable {
    entities: Vec<Entity>,
    capacity: usize,
    track_radius: f64,
}

impl EntityTable {
    /// Creates an empty table.
    pub fn new(capacity: usize, track_radius: f64) -> Self {
        Self {
            entities: Vec::with_capacity(capacity.min(64)),
            capacity: capacity.max(1),
            track_radius,
        }
    }

    /// Number of tracked entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Iterates entities.
    pub fn iter(&self) -> impl Iterator<Item = &Entity> {
        self.entities.iter()
    }

    /// Looks up by runtime id.
    pub fn get(&self, runtime_id: u64) -> Option<&Entity> {
        self.entities.iter().find(|e| e.runtime_id == runtime_id)
    }

    /// Mutable lookup by runtime id.
    pub fn get_mut(&mut self, runtime_id: u64) -> Option<&mut Entity> {
        self.entities
            .iter_mut()
            .find(|e| e.runtime_id == runtime_id)
    }

    /// Player by name (case-insensitive).
    pub fn player(&self, name: &str) -> Option<&Entity> {
        self.entities.iter().find(|e| {
            e.username
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
    }

    /// Adds or replaces an entity if it lies within range of `center`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &mut self,
        center: Vec3,
        runtime_id: u64,
        unique_id: i64,
        kind: &str,
        username: Option<&str>,
        position: Vec3,
        velocity: Vec3,
        yaw: f32,
        pitch: f32,
        item: Option<(i32, u16)>,
    ) -> bool {
        let dist = position.distance(center);
        let is_player = username.is_some();
        if dist > self.track_radius && !is_player {
            return false;
        }
        let entity = Entity {
            runtime_id,
            unique_id,
            kind: intern(kind),
            username: username.map(Into::into),
            position,
            velocity,
            yaw,
            pitch,
            on_ground: false,
            item,
            held_item: 0,
        };
        if let Some(slot) = self.get_mut(runtime_id) {
            *slot = entity;
            return true;
        }
        if self.entities.len() >= self.capacity {
            // Replace the farthest entity if the new one is closer.
            let (idx, far) = self
                .entities
                .iter()
                .enumerate()
                .map(|(i, e)| (i, e.position.distance(center)))
                .fold((0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a });
            if far <= dist {
                return false;
            }
            self.entities.swap_remove(idx);
        }
        self.entities.push(entity);
        true
    }

    /// Removes by unique id (falls back to runtime id equality).
    pub fn remove_unique(&mut self, unique_id: i64) -> Option<Entity> {
        let idx = self
            .entities
            .iter()
            .position(|e| e.unique_id == unique_id)
            .or_else(|| {
                self.entities
                    .iter()
                    .position(|e| e.runtime_id == unique_id as u64)
            })?;
        Some(self.entities.swap_remove(idx))
    }

    /// Removes by runtime id.
    pub fn remove_runtime(&mut self, runtime_id: u64) -> Option<Entity> {
        let idx = self
            .entities
            .iter()
            .position(|e| e.runtime_id == runtime_id)?;
        Some(self.entities.swap_remove(idx))
    }

    /// Evicts non-player entities farther than the tracking radius.
    pub fn prune(&mut self, center: Vec3) -> usize {
        let before = self.entities.len();
        let r = self.track_radius;
        self.entities
            .retain(|e| e.is_player() || e.position.distance(center) <= r);
        before - self.entities.len()
    }

    /// Nearest entity matching `filter`.
    pub fn nearest(&self, center: Vec3, filter: impl Fn(&Entity) -> bool) -> Option<&Entity> {
        self.entities.iter().filter(|e| filter(e)).min_by(|a, b| {
            a.position
                .distance(center)
                .partial_cmp(&b.position.distance(center))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Heap bytes owned.
    pub fn heap_bytes(&self) -> usize {
        self.entities.capacity() * std::mem::size_of::<Entity>()
            + self
                .entities
                .iter()
                .map(|e| e.username.as_ref().map_or(0, |n| n.len()))
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_and_pruned() {
        let mut t = EntityTable::new(2, 16.0);
        let c = Vec3::ZERO;
        assert!(t.spawn(
            c,
            1,
            1,
            "minecraft:zombie",
            None,
            Vec3::new(5.0, 0.0, 0.0),
            Vec3::ZERO,
            0.0,
            0.0,
            None
        ));
        assert!(t.spawn(
            c,
            2,
            2,
            "minecraft:cow",
            None,
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::ZERO,
            0.0,
            0.0,
            None
        ));
        assert!(!t.spawn(
            c,
            3,
            3,
            "minecraft:pig",
            None,
            Vec3::new(40.0, 0.0, 0.0),
            Vec3::ZERO,
            0.0,
            0.0,
            None
        ));
        assert!(t.spawn(
            c,
            4,
            4,
            "minecraft:pig",
            None,
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::ZERO,
            0.0,
            0.0,
            None
        ));
        assert!(t.get(2).is_none());
        assert_eq!(t.nearest(c, |_| true).unwrap().runtime_id, 4);
        t.get_mut(1).unwrap().position = Vec3::new(30.0, 0.0, 0.0);
        assert_eq!(t.prune(c), 1);
        assert!(t.remove_unique(4).is_some());
        assert!(t.is_empty());
    }
}
