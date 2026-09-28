//! Memory-bounded sliding voxel window centred on the bot.
//!
//! Storage is a fixed toroidal grid of `(2r+1)²` chunk columns. Each column
//! keeps its sub-chunks in one of three fidelity levels:
//!
//! * **hot** — full paletted block data (within `hot_radius` sub-chunks of the
//!   bot vertically): used for block lookups, digging and `find_blocks`;
//! * **cold** — a 512-byte collision bitmask: enough for physics and
//!   pathfinding, but block identity is dropped;
//! * **air / unknown / requested** — no heap storage at all.
//!
//! Columns leaving the horizontal window are evicted immediately.

use std::sync::Arc;

use crate::block::{BlockFlags, Shape};
use crate::chunk::{min_sub_chunk_y, LevelChunk, SubChunk, SubChunkMode};
use crate::palette::local_index;
use crate::registry::{BlockRef, BlockRegistry};
use torchflower_protocol_core::wire::WireError;

/// Integer block coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BlockPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl BlockPos {
    /// Creates a position.
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// Block containing the given world coordinates.
    pub fn from_f64(x: f64, y: f64, z: f64) -> Self {
        Self::new(x.floor() as i32, y.floor() as i32, z.floor() as i32)
    }

    /// Offset copy.
    pub const fn offset(self, dx: i32, dy: i32, dz: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.z + dz)
    }

    /// Neighbour across a Bedrock face id (0 down, 1 up, 2 north, 3 south, 4 west, 5 east).
    pub fn neighbor(self, face: u8) -> Self {
        let [dx, dy, dz] = face_normal(face);
        self.offset(dx, dy, dz)
    }

    /// Block centre.
    pub fn center(self) -> [f64; 3] {
        [
            self.x as f64 + 0.5,
            self.y as f64 + 0.5,
            self.z as f64 + 0.5,
        ]
    }

    /// Squared distance between block positions.
    pub fn dist_sq(self, o: BlockPos) -> i64 {
        let dx = (self.x - o.x) as i64;
        let dy = (self.y - o.y) as i64;
        let dz = (self.z - o.z) as i64;
        dx * dx + dy * dy + dz * dz
    }

    /// Chunk column coordinates.
    pub fn chunk(self) -> (i32, i32) {
        (self.x >> 4, self.z >> 4)
    }

    /// As array.
    pub fn to_array(self) -> [i32; 3] {
        [self.x, self.y, self.z]
    }
}

/// Unit normal of a Bedrock face id.
pub fn face_normal(face: u8) -> [i32; 3] {
    match face {
        0 => [0, -1, 0],
        1 => [0, 1, 0],
        2 => [0, 0, -1],
        3 => [0, 0, 1],
        4 => [-1, 0, 0],
        _ => [1, 0, 0],
    }
}

/// Window configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowConfig {
    /// Horizontal radius in chunks (window is `(2r+1)²` columns).
    pub radius_chunks: u8,
    /// Vertical radius, in sub-chunks, that keeps full block data.
    pub hot_radius: u8,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            radius_chunks: 2,
            hot_radius: 1,
        }
    }
}

enum SubSlot {
    Unknown,
    Requested,
    Air,
    Hot(SubChunk),
    Cold(Box<[u64; 64]>),
    /// Cold data in a request-mode column that should be refreshed.
    ColdStale(Box<[u64; 64]>),
}

struct Column {
    cx: i32,
    cz: i32,
    request_mode: bool,
    /// Highest requestable sub-chunk (absolute index) for request mode.
    highest: i8,
    subs: Vec<SubSlot>,
}

/// Sparse world window. One per bot.
pub struct SparseWorld {
    registry: Arc<BlockRegistry>,
    cfg: WindowConfig,
    dimension: i32,
    min_sub: i8,
    max_sub: i8,
    center_chunk: (i32, i32),
    center_sub: i8,
    columns: Vec<Option<Column>>,
}

/// Outcome of inserting a `LevelChunk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkInsert {
    /// Column is outside the window and was dropped.
    Ignored,
    /// Inline data stored; the number of retained sub-chunks is attached.
    Stored(u32),
    /// Column needs `SubChunkRequest`s.
    NeedsRequest,
}

impl SparseWorld {
    /// Creates an empty window.
    pub fn new(registry: Arc<BlockRegistry>, cfg: WindowConfig) -> Self {
        let w = 2 * cfg.radius_chunks as usize + 1;
        let mut columns = Vec::with_capacity(w * w);
        columns.resize_with(w * w, || None);
        let mut world = Self {
            registry,
            cfg,
            dimension: 0,
            min_sub: -4,
            max_sub: 19,
            center_chunk: (0, 0),
            center_sub: 4,
            columns,
        };
        world.set_dimension(0);
        world
    }

    /// Registry shared by this world.
    pub fn registry(&self) -> &Arc<BlockRegistry> {
        &self.registry
    }

    /// Current dimension.
    pub fn dimension(&self) -> i32 {
        self.dimension
    }

    /// Switches dimension, clearing all data.
    pub fn set_dimension(&mut self, dimension: i32) {
        self.dimension = dimension;
        self.min_sub = min_sub_chunk_y(dimension);
        self.max_sub = match dimension {
            0 => 19,
            1 => 7,
            _ => 15,
        };
        self.columns.iter_mut().for_each(|c| *c = None);
    }

    fn width(&self) -> i32 {
        2 * self.cfg.radius_chunks as i32 + 1
    }

    fn slot_index(&self, cx: i32, cz: i32) -> usize {
        let w = self.width();
        (cx.rem_euclid(w) * w + cz.rem_euclid(w)) as usize
    }

    /// True if the chunk column lies in the window.
    pub fn in_window(&self, cx: i32, cz: i32) -> bool {
        let r = self.cfg.radius_chunks as i32;
        (cx - self.center_chunk.0).abs() <= r && (cz - self.center_chunk.1).abs() <= r
    }

    fn is_hot(&self, sy: i8) -> bool {
        (sy as i32 - self.center_sub as i32).abs() <= self.cfg.hot_radius as i32
    }

    fn column(&self, cx: i32, cz: i32) -> Option<&Column> {
        if !self.in_window(cx, cz) {
            return None;
        }
        self.columns[self.slot_index(cx, cz)]
            .as_ref()
            .filter(|c| c.cx == cx && c.cz == cz)
    }

    fn column_mut(&mut self, cx: i32, cz: i32) -> Option<&mut Column> {
        if !self.in_window(cx, cz) {
            return None;
        }
        let idx = self.slot_index(cx, cz);
        self.columns[idx]
            .as_mut()
            .filter(|c| c.cx == cx && c.cz == cz)
    }

    /// True if column data has been received.
    pub fn is_column_loaded(&self, cx: i32, cz: i32) -> bool {
        self.column(cx, cz).is_some()
    }

    /// Moves the window centre. Evicts columns outside the window and demotes
    /// sub-chunks outside the hot band to collision masks.
    pub fn set_center(&mut self, pos: BlockPos) {
        let chunk = pos.chunk();
        let sub = (pos.y >> 4).clamp(self.min_sub as i32, self.max_sub as i32) as i8;
        if chunk == self.center_chunk && sub == self.center_sub {
            return;
        }
        self.center_chunk = chunk;
        self.center_sub = sub;
        let r = self.cfg.radius_chunks as i32;
        let min_sub = self.min_sub;
        let hot = self.cfg.hot_radius as i32;
        let registry = self.registry.clone();
        for slot in self.columns.iter_mut() {
            let Some(col) = slot else { continue };
            if (col.cx - chunk.0).abs() > r || (col.cz - chunk.1).abs() > r {
                *slot = None;
                continue;
            }
            let request_mode = col.request_mode;
            for (i, s) in col.subs.iter_mut().enumerate() {
                let sy = min_sub as i32 + i as i32;
                let is_hot = (sy - sub as i32).abs() <= hot;
                match s {
                    SubSlot::Hot(data) if !is_hot => {
                        if let Some(mask) = to_mask(&registry, data) {
                            *s = SubSlot::Cold(mask);
                        }
                    }
                    SubSlot::Cold(_) if is_hot && request_mode => {
                        if let SubSlot::Cold(mask) = std::mem::replace(s, SubSlot::Unknown) {
                            *s = SubSlot::ColdStale(mask);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn sub_index(&self, sy: i32) -> Option<usize> {
        if sy < self.min_sub as i32 || sy > self.max_sub as i32 {
            return None;
        }
        Some((sy - self.min_sub as i32) as usize)
    }

    fn ensure_column(&mut self, cx: i32, cz: i32) -> Option<&mut Column> {
        if !self.in_window(cx, cz) {
            return None;
        }
        let idx = self.slot_index(cx, cz);
        let count = (self.max_sub - self.min_sub + 1) as usize;
        let replace = !matches!(&self.columns[idx], Some(c) if c.cx == cx && c.cz == cz);
        if replace {
            let mut subs = Vec::with_capacity(count);
            subs.resize_with(count, || SubSlot::Unknown);
            self.columns[idx] = Some(Column {
                cx,
                cz,
                request_mode: false,
                highest: self.max_sub,
                subs,
            });
        }
        self.columns[idx].as_mut()
    }

    /// Stores a decoded `LevelChunk`.
    pub fn insert_level_chunk(&mut self, chunk: &LevelChunk<'_>) -> Result<ChunkInsert, WireError> {
        if chunk.dimension != self.dimension || !self.in_window(chunk.chunk_x, chunk.chunk_z) {
            return Ok(ChunkInsert::Ignored);
        }
        let (cx, cz) = (chunk.chunk_x, chunk.chunk_z);
        let min_sub = self.min_sub;
        let max_sub = self.max_sub;
        match chunk.mode {
            SubChunkMode::RequestLimitless | SubChunkMode::RequestLimited { .. } => {
                let highest = match chunk.mode {
                    SubChunkMode::RequestLimited { highest } => {
                        (min_sub as i32 + highest as i32).min(max_sub as i32) as i8
                    }
                    _ => max_sub,
                };
                let col = self.ensure_column(cx, cz).expect("in window");
                col.request_mode = true;
                col.highest = highest;
                for s in col.subs.iter_mut() {
                    *s = SubSlot::Unknown;
                }
                Ok(ChunkInsert::NeedsRequest)
            }
            SubChunkMode::Inline(_) => {
                let air = self.registry.air_runtime_id();
                let mut decoded: Vec<(i8, SubChunk)> = Vec::new();
                chunk.for_each_sub_chunk(
                    min_sub,
                    air,
                    |y| y >= min_sub && y <= max_sub,
                    |y, s| decoded.push((y, s)),
                )?;
                let kept = decoded.len() as u32;
                {
                    let col = self.ensure_column(cx, cz).expect("in window");
                    col.request_mode = false;
                    for s in col.subs.iter_mut() {
                        *s = SubSlot::Air;
                    }
                }
                for (y, sub) in decoded {
                    self.store_sub(cx, y as i32, cz, sub);
                }
                Ok(ChunkInsert::Stored(kept))
            }
        }
    }

    fn store_sub(&mut self, cx: i32, sy: i32, cz: i32, sub: SubChunk) {
        let Some(i) = self.sub_index(sy) else { return };
        let hot = self.is_hot(sy as i8);
        let air = self.registry.air_runtime_id();
        let slot = if sub.layers[1].is_none()
            && sub.layers[0]
                .as_ref()
                .is_some_and(|l| l.is_uniform() && Some(l.palette()[0]) == air)
        {
            SubSlot::Air
        } else if hot || sub.layers[0].as_ref().is_some_and(|l| l.is_uniform()) {
            SubSlot::Hot(sub)
        } else {
            match to_mask(&self.registry, &sub) {
                Some(mask) => SubSlot::Cold(mask),
                None => SubSlot::Hot(sub),
            }
        };
        if let Some(col) = self.column_mut(cx, cz) {
            col.subs[i] = slot;
        }
    }

    /// Stores one sub-chunk received through `SubChunk` (absolute coords).
    pub fn insert_sub_chunk(&mut self, cx: i32, sy: i32, cz: i32, sub: SubChunk) {
        if self.column(cx, cz).is_none() {
            if !self.in_window(cx, cz) {
                return;
            }
            self.ensure_column(cx, cz);
            if let Some(col) = self.column_mut(cx, cz) {
                col.request_mode = true;
            }
        }
        self.store_sub(cx, sy, cz, sub);
    }

    /// Marks a sub-chunk as entirely air.
    pub fn insert_air_sub_chunk(&mut self, cx: i32, sy: i32, cz: i32) {
        let Some(i) = self.sub_index(sy) else { return };
        if let Some(col) = self.column_mut(cx, cz) {
            col.subs[i] = SubSlot::Air;
        }
    }

    /// Marks a sub-chunk as permanently unavailable (e.g. out of bounds) so it
    /// is not requested again.
    pub fn mark_sub_chunk_unavailable(&mut self, cx: i32, sy: i32, cz: i32) {
        self.insert_air_sub_chunk(cx, sy, cz);
    }

    /// Returns up to `limit` sub-chunk positions that should be requested,
    /// nearest first, and marks them as requested.
    pub fn take_sub_chunk_requests(&mut self, limit: usize) -> Vec<[i32; 3]> {
        let (ccx, ccz) = self.center_chunk;
        let csub = self.center_sub as i32;
        let min_sub = self.min_sub as i32;
        let mut wanted: Vec<(i64, [i32; 3])> = Vec::new();
        for col in self.columns.iter().flatten() {
            if !col.request_mode {
                continue;
            }
            for (i, s) in col.subs.iter().enumerate() {
                let sy = min_sub + i as i32;
                if sy > col.highest as i32 {
                    continue;
                }
                let need = matches!(s, SubSlot::Unknown | SubSlot::ColdStale(_));
                if need {
                    let d = ((col.cx - ccx) as i64).pow(2)
                        + ((col.cz - ccz) as i64).pow(2)
                        + ((sy - csub) as i64).pow(2);
                    wanted.push((d, [col.cx, sy, col.cz]));
                }
            }
        }
        wanted.sort_unstable_by_key(|w| w.0);
        wanted.truncate(limit);
        let out: Vec<[i32; 3]> = wanted.into_iter().map(|w| w.1).collect();
        for p in &out {
            if let (Some(i), Some(col)) = (self.sub_index(p[1]), self.column_mut(p[0], p[2])) {
                if matches!(col.subs[i], SubSlot::Unknown) {
                    col.subs[i] = SubSlot::Requested;
                }
            }
        }
        out
    }

    /// Re-queues sub-chunks whose request never got an answer.
    pub fn reset_pending_requests(&mut self) {
        for col in self.columns.iter_mut().flatten() {
            for s in col.subs.iter_mut() {
                if matches!(s, SubSlot::Requested) {
                    *s = SubSlot::Unknown;
                }
            }
        }
    }

    fn sub_at(&self, pos: BlockPos) -> Option<&SubSlot> {
        let (cx, cz) = pos.chunk();
        let i = self.sub_index(pos.y >> 4)?;
        self.column(cx, cz).map(|c| &c.subs[i])
    }

    /// Runtime id at `pos` on the given layer, if full data is present.
    pub fn runtime_id_layer(&self, pos: BlockPos, layer: usize) -> Option<u32> {
        let (x, y, z) = ((pos.x & 15) as u8, (pos.y & 15) as u8, (pos.z & 15) as u8);
        match self.sub_at(pos)? {
            SubSlot::Air => {
                if layer == 0 {
                    self.registry.air_runtime_id()
                } else {
                    None
                }
            }
            SubSlot::Hot(sub) => sub.get(x, y, z, layer),
            _ => None,
        }
    }

    /// Runtime id at `pos` (layer 0).
    pub fn runtime_id(&self, pos: BlockPos) -> Option<u32> {
        self.runtime_id_layer(pos, 0)
    }

    /// Resolved block at `pos`, if full data is present.
    pub fn block_at(&self, pos: BlockPos) -> Option<BlockRef<'_>> {
        self.runtime_id(pos).map(|rid| self.registry.get(rid))
    }

    /// Collision shape at `pos`. `None` means the data is not loaded; callers
    /// should treat that as solid for safety.
    pub fn shape_at(&self, pos: BlockPos) -> Option<Shape> {
        let (x, y, z) = ((pos.x & 15) as u8, (pos.y & 15) as u8, (pos.z & 15) as u8);
        if pos.y < self.min_sub as i32 * 16 {
            return Some(Shape::FULL);
        }
        if pos.y > self.max_sub as i32 * 16 + 15 {
            return Some(Shape::Empty);
        }
        match self.sub_at(pos)? {
            SubSlot::Air => Some(Shape::Empty),
            SubSlot::Hot(sub) => sub
                .get(x, y, z, 0)
                .map(|rid| self.registry.get(rid).shape()),
            SubSlot::Cold(mask) | SubSlot::ColdStale(mask) => {
                let i = local_index(x, y, z);
                Some(if mask[i / 64] >> (i % 64) & 1 == 1 {
                    Shape::FULL
                } else {
                    Shape::Empty
                })
            }
            SubSlot::Unknown | SubSlot::Requested => None,
        }
    }

    /// Material flags at `pos` combining both layers (e.g. water-logging).
    pub fn flags_at(&self, pos: BlockPos) -> Option<BlockFlags> {
        let base = self.runtime_id_layer(pos, 0)?;
        let mut flags = self.registry.get(base).flags();
        if let Some(l1) = self.runtime_id_layer(pos, 1) {
            flags = flags | self.registry.get(l1).flags().without(BlockFlags::SOLID);
        }
        Some(flags)
    }

    /// Applies an `UpdateBlock`.
    pub fn set_block(&mut self, pos: BlockPos, runtime_id: u32, layer: usize) {
        let (cx, cz) = pos.chunk();
        let Some(i) = self.sub_index(pos.y >> 4) else {
            return;
        };
        let air = self.registry.air_runtime_id();
        let solid = self.registry.get(runtime_id).is_solid();
        let (x, y, z) = ((pos.x & 15) as u8, (pos.y & 15) as u8, (pos.z & 15) as u8);
        let idx = local_index(x, y, z);
        let Some(col) = self.column_mut(cx, cz) else {
            return;
        };
        let slot = &mut col.subs[i];
        if matches!(slot, SubSlot::Air) {
            match air {
                Some(air) if Some(runtime_id) != Some(air) || layer != 0 => {
                    *slot = SubSlot::Hot(SubChunk::uniform(air));
                }
                _ => return,
            }
        }
        match slot {
            SubSlot::Hot(sub) => {
                if layer >= 2 {
                    return;
                }
                if sub.layers[layer].is_none() {
                    let Some(air) = air else { return };
                    sub.layers[layer] = Some(crate::palette::PalettedStorage::uniform(air));
                }
                if let Some(storage) = sub.layers[layer].as_mut() {
                    storage.set(idx, runtime_id);
                }
            }
            SubSlot::Cold(mask) | SubSlot::ColdStale(mask) if layer == 0 => {
                if solid {
                    mask[idx / 64] |= 1 << (idx % 64);
                } else {
                    mask[idx / 64] &= !(1 << (idx % 64));
                }
            }
            _ => {}
        }
    }

    /// Finds blocks whose runtime id is in `ids` within `max_distance` of
    /// `origin` (hot data only), nearest first.
    pub fn find_blocks(
        &self,
        origin: BlockPos,
        ids: &[u32],
        max_distance: u32,
        limit: usize,
    ) -> Vec<BlockPos> {
        if ids.is_empty() || limit == 0 {
            return Vec::new();
        }
        let max_sq = (max_distance as i64).pow(2);
        let mut found: Vec<(i64, BlockPos)> = Vec::new();
        for col in self.columns.iter().flatten() {
            for (i, s) in col.subs.iter().enumerate() {
                let SubSlot::Hot(sub) = s else { continue };
                let Some(layer) = sub.layers[0].as_ref() else {
                    continue;
                };
                if !layer.palette().iter().any(|p| ids.contains(p)) {
                    continue;
                }
                let sy = self.min_sub as i32 + i as i32;
                for x in 0..16u8 {
                    for z in 0..16u8 {
                        for y in 0..16u8 {
                            let rid = layer.get(local_index(x, y, z));
                            if !ids.contains(&rid) {
                                continue;
                            }
                            let p = BlockPos::new(
                                col.cx * 16 + x as i32,
                                sy * 16 + y as i32,
                                col.cz * 16 + z as i32,
                            );
                            let d = p.dist_sq(origin);
                            if d <= max_sq {
                                found.push((d, p));
                            }
                        }
                    }
                }
            }
        }
        found.sort_unstable_by_key(|f| f.0);
        found.truncate(limit);
        found.into_iter().map(|f| f.1).collect()
    }

    /// Heap bytes owned by this window (excludes the shared registry).
    pub fn heap_bytes(&self) -> usize {
        let mut total = self.columns.capacity() * std::mem::size_of::<Option<Column>>();
        for col in self.columns.iter().flatten() {
            total += col.subs.capacity() * std::mem::size_of::<SubSlot>();
            for s in &col.subs {
                total += match s {
                    SubSlot::Hot(sub) => sub.heap_bytes(),
                    SubSlot::Cold(_) | SubSlot::ColdStale(_) => 512,
                    _ => 0,
                };
            }
        }
        total
    }

    /// Counts `(hot, cold)` sub-chunks currently stored.
    pub fn sub_chunk_counts(&self) -> (usize, usize) {
        let mut hot = 0;
        let mut cold = 0;
        for col in self.columns.iter().flatten() {
            for s in &col.subs {
                match s {
                    SubSlot::Hot(_) => hot += 1,
                    SubSlot::Cold(_) | SubSlot::ColdStale(_) => cold += 1,
                    _ => {}
                }
            }
        }
        (hot, cold)
    }

    /// Lowest and highest valid block Y for the current dimension.
    pub fn y_range(&self) -> (i32, i32) {
        (self.min_sub as i32 * 16, self.max_sub as i32 * 16 + 15)
    }
}

fn to_mask(registry: &BlockRegistry, sub: &SubChunk) -> Option<Box<[u64; 64]>> {
    let layer = sub.layers[0].as_ref()?;
    if layer.is_uniform() {
        return None;
    }
    let solid: Vec<bool> = layer
        .palette()
        .iter()
        .map(|rid| registry.get(*rid).is_solid())
        .collect();
    let mut mask = Box::new([0u64; 64]);
    for i in 0..4096 {
        let rid = layer.get(i);
        let idx = layer.palette().iter().position(|p| *p == rid).unwrap_or(0);
        if solid[idx] {
            mask[i / 64] |= 1 << (i % 64);
        }
    }
    Some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RuntimeIdMode;
    use torchflower_protocol_core::wire::{put_var_i32, put_var_u32};

    fn registry() -> Arc<BlockRegistry> {
        Arc::new(BlockRegistry::from_states(
            vec![
                ("minecraft:air".to_string(), None),
                ("minecraft:stone".to_string(), None),
                ("minecraft:iron_ore".to_string(), None),
            ],
            RuntimeIdMode::Hashed,
        ))
    }

    fn rid(reg: &BlockRegistry, name: &str) -> u32 {
        reg.runtime_ids_for_name(name)[0]
    }

    fn level_chunk(cx: i32, cz: i32, subs: &[(i8, SubChunk)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (y, s) in subs {
            s.encode(*y, &mut body);
        }
        let mut pkt = Vec::new();
        put_var_i32(&mut pkt, cx);
        put_var_i32(&mut pkt, cz);
        put_var_i32(&mut pkt, 0);
        put_var_u32(&mut pkt, subs.len() as u32);
        pkt.push(0);
        put_var_u32(&mut pkt, body.len() as u32);
        pkt.extend_from_slice(&body);
        pkt
    }

    fn terrain(reg: &BlockRegistry) -> SubChunk {
        let stone = rid(reg, "stone");
        let air = rid(reg, "air");
        let ore = rid(reg, "iron_ore");
        let mut sub = SubChunk::uniform(air);
        let layer = sub.layers[0].as_mut().unwrap();
        for x in 0..16 {
            for z in 0..16 {
                for y in 0..4 {
                    layer.set(local_index(x, y, z), stone);
                }
            }
        }
        layer.set(local_index(5, 2, 5), ore);
        sub
    }

    #[test]
    fn stores_queries_and_finds() {
        let reg = registry();
        let mut world = SparseWorld::new(reg.clone(), WindowConfig::default());
        world.set_center(BlockPos::new(8, 64, 8));
        let pkt = level_chunk(0, 0, &[(4, terrain(&reg))]);
        let chunk = LevelChunk::decode(&pkt).unwrap();
        assert_eq!(
            world.insert_level_chunk(&chunk).unwrap(),
            ChunkInsert::Stored(1)
        );
        assert_eq!(
            world.block_at(BlockPos::new(1, 65, 1)).unwrap().name(),
            "minecraft:stone"
        );
        assert!(world.block_at(BlockPos::new(1, 70, 1)).unwrap().is_air());
        // Above the inline sub-chunks is air.
        assert_eq!(world.shape_at(BlockPos::new(1, 200, 1)), Some(Shape::Empty));
        let ore = world.find_blocks(BlockPos::new(8, 64, 8), &[rid(&reg, "iron_ore")], 32, 4);
        assert_eq!(ore, vec![BlockPos::new(5, 66, 5)]);
        world.set_block(BlockPos::new(5, 66, 5), rid(&reg, "air"), 0);
        assert!(world.block_at(BlockPos::new(5, 66, 5)).unwrap().is_air());
    }

    #[test]
    fn out_of_window_dropped_and_evicted() {
        let reg = registry();
        let mut world = SparseWorld::new(reg.clone(), WindowConfig::default());
        world.set_center(BlockPos::new(8, 64, 8));
        let far = level_chunk(10, 0, &[(4, terrain(&reg))]);
        assert_eq!(
            world
                .insert_level_chunk(&LevelChunk::decode(&far).unwrap())
                .unwrap(),
            ChunkInsert::Ignored
        );
        let near = level_chunk(1, 1, &[(4, terrain(&reg))]);
        world
            .insert_level_chunk(&LevelChunk::decode(&near).unwrap())
            .unwrap();
        assert!(world.is_column_loaded(1, 1));
        world.set_center(BlockPos::new(16 * 8, 64, 8));
        assert!(!world.is_column_loaded(1, 1));
        assert_eq!(world.sub_chunk_counts(), (0, 0));
    }

    #[test]
    fn vertical_demotion_keeps_collision() {
        let reg = registry();
        let mut world = SparseWorld::new(reg.clone(), WindowConfig::default());
        world.set_center(BlockPos::new(8, 64, 8));
        let pkt = level_chunk(0, 0, &[(4, terrain(&reg))]);
        world
            .insert_level_chunk(&LevelChunk::decode(&pkt).unwrap())
            .unwrap();
        assert_eq!(world.sub_chunk_counts(), (1, 0));
        world.set_center(BlockPos::new(8, 160, 8));
        assert_eq!(world.sub_chunk_counts(), (0, 1));
        assert_eq!(world.shape_at(BlockPos::new(1, 65, 1)), Some(Shape::FULL));
        assert_eq!(world.shape_at(BlockPos::new(1, 70, 1)), Some(Shape::Empty));
        assert!(world.block_at(BlockPos::new(1, 65, 1)).is_none());
    }

    #[test]
    fn request_mode_requests_nearest_first() {
        let reg = registry();
        let mut world = SparseWorld::new(reg.clone(), WindowConfig::default());
        world.set_center(BlockPos::new(8, 64, 8));
        let mut pkt = Vec::new();
        put_var_i32(&mut pkt, 0);
        put_var_i32(&mut pkt, 0);
        put_var_i32(&mut pkt, 0);
        put_var_u32(&mut pkt, u32::MAX);
        pkt.push(0);
        put_var_u32(&mut pkt, 0);
        let chunk = LevelChunk::decode(&pkt).unwrap();
        assert_eq!(
            world.insert_level_chunk(&chunk).unwrap(),
            ChunkInsert::NeedsRequest
        );
        let req = world.take_sub_chunk_requests(3);
        assert_eq!(req[0], [0, 4, 0]);
        assert_eq!(req.len(), 3);
        assert!(world.shape_at(BlockPos::new(1, 65, 1)).is_none());
        world.insert_sub_chunk(0, 4, 0, terrain(&reg));
        assert_eq!(world.shape_at(BlockPos::new(1, 65, 1)), Some(Shape::FULL));
        assert!(!world.take_sub_chunk_requests(64).contains(&[0, 4, 0]));
    }
}
