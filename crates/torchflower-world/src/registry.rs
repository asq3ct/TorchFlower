//! Runtime-id → block-state registry.
//!
//! Bedrock servers identify block states by *network runtime ids*, which are
//! either sequential indices into the canonical palette (sorted by the FNV-1
//! 64-bit hash of the block name) or, when `block_network_ids_are_hashes` is
//! set in `StartGame`, FNV-1a 32-bit hashes of the state NBT.
//!
//! The registry is immutable after construction and meant to be shared by all
//! bots on the same protocol version through an `Arc`, so its size does not
//! count against the per-bot memory budget.

use std::collections::HashMap;

use torchflower_protocol_core::wire::{NbtFlavor, NbtValue, WireError, WireReader};

use crate::block::{
    block_info, classify, liquid_depth, shape_for_state, BlockFlags, BlockInfo, Material, Shape,
    ToolKind,
};

/// How network runtime ids are assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeIdMode {
    /// Index into the canonical palette sorted by FNV-1 64 of the name.
    Sequential,
    /// FNV-1a 32 hash of the `{name, states}` little-endian NBT compound.
    Hashed,
}

#[derive(Debug)]
struct NameEntry {
    name: Box<str>,
    info: Option<&'static BlockInfo>,
    material: Material,
}

#[derive(Debug, Clone, Copy)]
struct StateEntry {
    name: u16,
    shape: Shape,
    liquid_depth: u8,
}

/// Immutable block registry.
#[derive(Debug)]
pub struct BlockRegistry {
    mode: RuntimeIdMode,
    names: Vec<NameEntry>,
    states: Vec<StateEntry>,
    hash_index: HashMap<u32, u32>,
    air_runtime_id: Option<u32>,
}

static UNKNOWN_MATERIAL: Material = Material {
    flags: BlockFlags(BlockFlags::SOLID.0 | BlockFlags::UNKNOWN.0),
    shape: Shape::FULL,
    tool: ToolKind::None,
    harvest: 0,
};

/// FNV-1a 32-bit hash of a block state, as used by hashed runtime ids.
pub fn network_block_hash(name: &str, states: Option<&NbtValue>) -> u32 {
    if name == "minecraft:unknown" {
        return 0xffff_fffe;
    }
    let mut sorted = match states {
        Some(NbtValue::Compound(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let root = NbtValue::Compound(vec![
        ("name".to_string(), NbtValue::String(name.to_string())),
        ("states".to_string(), NbtValue::Compound(sorted)),
    ]);
    let mut buf = Vec::with_capacity(64);
    root.write_le_root("", &mut buf);
    let mut h: u32 = 0x811c_9dc5;
    for b in buf {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn fnv1_64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        h ^= b as u64;
    }
    h
}

impl BlockRegistry {
    /// Builds a registry from block states in canonical order.
    ///
    /// In [`RuntimeIdMode::Sequential`] the states are stably re-sorted by the
    /// FNV-1 64 hash of their name, matching BDS ≥ 1.18.30.
    pub fn from_states<I>(states: I, mode: RuntimeIdMode) -> Self
    where
        I: IntoIterator<Item = (String, Option<NbtValue>)>,
    {
        let mut raw: Vec<(String, Option<NbtValue>)> = states.into_iter().collect();
        if mode == RuntimeIdMode::Sequential {
            raw.sort_by_cached_key(|(name, _)| fnv1_64(name.as_bytes()));
        }
        let mut reg = BlockRegistry {
            mode,
            names: Vec::new(),
            states: Vec::with_capacity(raw.len()),
            hash_index: HashMap::new(),
            air_runtime_id: None,
        };
        let mut name_ids: HashMap<String, u16> = HashMap::new();
        for (index, (name, states)) in raw.into_iter().enumerate() {
            let name_id = match name_ids.get(&name) {
                Some(id) => *id,
                None => {
                    let id = reg.names.len().min(u16::MAX as usize) as u16;
                    reg.names.push(NameEntry {
                        info: block_info(&name),
                        material: classify(&name),
                        name: name.clone().into_boxed_str(),
                    });
                    name_ids.insert(name.clone(), id);
                    id
                }
            };
            let base = reg.names[name_id as usize].material.shape;
            reg.states.push(StateEntry {
                name: name_id,
                shape: shape_for_state(&name, base, states.as_ref()),
                liquid_depth: liquid_depth(states.as_ref()),
            });
            let runtime_id = match mode {
                RuntimeIdMode::Sequential => index as u32,
                RuntimeIdMode::Hashed => {
                    let h = network_block_hash(&name, states.as_ref());
                    reg.hash_index.insert(h, index as u32);
                    h
                }
            };
            if reg.air_runtime_id.is_none() && name == "minecraft:air" {
                reg.air_runtime_id = Some(runtime_id);
            }
        }
        reg
    }

    /// Parses a concatenation of network-NBT block state compounds, e.g.
    /// `canonical_block_states.nbt` from pmmp/BedrockData for the server's
    /// protocol version.
    pub fn from_canonical_nbt(bytes: &[u8], mode: RuntimeIdMode) -> Result<Self, WireError> {
        let mut reader = WireReader::new(bytes);
        let mut states = Vec::new();
        while reader.remaining() > 0 {
            let (_, value) = reader.read_nbt(NbtFlavor::Network)?;
            let name = value
                .get("name")
                .and_then(NbtValue::as_str)
                .ok_or_else(|| reader.err("block state name"))?
                .to_string();
            let st = value.get("states").cloned();
            states.push((name, st));
        }
        Ok(Self::from_states(states, mode))
    }

    /// Minimal registry used when no canonical palette is available.
    ///
    /// In hashed mode air, water and lava are recognised exactly; everything
    /// else is reported as an unknown solid cube. In sequential mode nothing
    /// can be resolved without palette data, so call
    /// [`BlockRegistry::with_air_runtime_id`] if the air id is known.
    pub fn fallback(mode: RuntimeIdMode) -> Self {
        let mut states: Vec<(String, Option<NbtValue>)> = vec![(
            "minecraft:air".to_string(),
            Some(NbtValue::Compound(vec![])),
        )];
        for liquid in [
            "minecraft:water",
            "minecraft:flowing_water",
            "minecraft:lava",
            "minecraft:flowing_lava",
        ] {
            for depth in 0..16 {
                states.push((
                    liquid.to_string(),
                    Some(NbtValue::Compound(vec![(
                        "liquid_depth".to_string(),
                        NbtValue::Int(depth),
                    )])),
                ));
            }
        }
        match mode {
            RuntimeIdMode::Hashed => Self::from_states(states, mode),
            RuntimeIdMode::Sequential => BlockRegistry {
                mode,
                names: Vec::new(),
                states: Vec::new(),
                hash_index: HashMap::new(),
                air_runtime_id: None,
            },
        }
    }

    /// Overrides the runtime id treated as air.
    pub fn with_air_runtime_id(mut self, runtime_id: u32) -> Self {
        self.air_runtime_id = Some(runtime_id);
        if self.names.is_empty() {
            self.names.push(NameEntry {
                name: "minecraft:air".into(),
                info: block_info("air"),
                material: classify("air"),
            });
        }
        self
    }

    /// Runtime id mode.
    pub fn mode(&self) -> RuntimeIdMode {
        self.mode
    }

    /// Number of known states.
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// True if the registry knows no states.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Runtime id of `minecraft:air`, if known.
    pub fn air_runtime_id(&self) -> Option<u32> {
        self.air_runtime_id
    }

    fn state_index(&self, runtime_id: u32) -> Option<u32> {
        match self.mode {
            RuntimeIdMode::Sequential => {
                ((runtime_id as usize) < self.states.len()).then_some(runtime_id)
            }
            RuntimeIdMode::Hashed => self.hash_index.get(&runtime_id).copied(),
        }
    }

    /// Resolves a runtime id.
    pub fn get(&self, runtime_id: u32) -> BlockRef<'_> {
        let state = self
            .state_index(runtime_id)
            .map(|i| self.states[i as usize]);
        let air = self.air_runtime_id == Some(runtime_id);
        BlockRef {
            registry: self,
            runtime_id,
            state,
            air_override: air && state.is_none(),
        }
    }

    /// All runtime ids whose block name equals `name` (namespace optional).
    pub fn runtime_ids_for_name(&self, name: &str) -> Vec<u32> {
        let full = if name.contains(':') {
            name.to_string()
        } else {
            format!("minecraft:{name}")
        };
        let Some(name_id) = self.names.iter().position(|n| *n.name == *full) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match self.mode {
            RuntimeIdMode::Sequential => {
                for (i, s) in self.states.iter().enumerate() {
                    if s.name as usize == name_id {
                        out.push(i as u32);
                    }
                }
            }
            RuntimeIdMode::Hashed => {
                for (rid, idx) in &self.hash_index {
                    if self.states[*idx as usize].name as usize == name_id {
                        out.push(*rid);
                    }
                }
                out.sort_unstable();
            }
        }
        out
    }

    /// Approximate heap footprint (shared across bots).
    pub fn heap_bytes(&self) -> usize {
        self.states.capacity() * std::mem::size_of::<StateEntry>()
            + self.names.capacity() * std::mem::size_of::<NameEntry>()
            + self.names.iter().map(|n| n.name.len()).sum::<usize>()
            + self.hash_index.capacity() * 12
    }
}

/// Borrowed view of a resolved block state.
#[derive(Debug, Clone, Copy)]
pub struct BlockRef<'a> {
    registry: &'a BlockRegistry,
    runtime_id: u32,
    state: Option<StateEntry>,
    air_override: bool,
}

impl<'a> BlockRef<'a> {
    fn name_entry(&self) -> Option<&'a NameEntry> {
        if self.air_override {
            return self
                .registry
                .names
                .iter()
                .find(|n| &*n.name == "minecraft:air");
        }
        self.state.map(|s| &self.registry.names[s.name as usize])
    }

    fn material(&self) -> &'a Material {
        match self.name_entry() {
            Some(entry) => &entry.material,
            None if self.air_override => &AIR_MATERIAL,
            None => &UNKNOWN_MATERIAL,
        }
    }

    /// Network runtime id.
    pub fn runtime_id(&self) -> u32 {
        self.runtime_id
    }

    /// Full identifier (`minecraft:stone`) or `"unknown"`.
    pub fn name(&self) -> &'a str {
        match self.name_entry() {
            Some(entry) => &entry.name,
            None if self.air_override => "minecraft:air",
            None => "unknown",
        }
    }

    /// True if the registry recognised this runtime id.
    pub fn is_known(&self) -> bool {
        self.state.is_some() || self.air_override
    }

    /// Material classification.
    pub fn material_info(&self) -> Material {
        *self.material()
    }

    /// Flags.
    pub fn flags(&self) -> BlockFlags {
        self.material().flags
    }

    /// Collision shape of this exact state.
    pub fn shape(&self) -> Shape {
        match self.state {
            Some(s) => s.shape,
            None => self.material().shape,
        }
    }

    /// True for air-like states.
    pub fn is_air(&self) -> bool {
        self.flags().contains(BlockFlags::AIR)
    }

    /// True if the state has collision.
    pub fn is_solid(&self) -> bool {
        !self.shape().is_empty()
    }

    /// True for water.
    pub fn is_water(&self) -> bool {
        self.flags().contains(BlockFlags::WATER)
    }

    /// True for lava.
    pub fn is_lava(&self) -> bool {
        self.flags().contains(BlockFlags::LAVA)
    }

    /// Liquid depth state (0 = source).
    pub fn liquid_depth(&self) -> u8 {
        self.state.map(|s| s.liquid_depth).unwrap_or(0)
    }

    /// Hardness (unknown blocks report 1.5; unbreakable blocks are negative).
    pub fn hardness(&self) -> f32 {
        if self.flags().contains(BlockFlags::UNBREAKABLE) {
            return -1.0;
        }
        self.name_entry()
            .and_then(|e| e.info)
            .map(|i| i.hardness)
            .unwrap_or(1.5)
    }

    /// Surface friction (0.6 default).
    pub fn friction(&self) -> f32 {
        self.name_entry()
            .and_then(|e| e.info)
            .map(|i| i.friction)
            .unwrap_or(0.6)
    }

    /// Light emission.
    pub fn light(&self) -> u8 {
        self.name_entry()
            .and_then(|e| e.info)
            .map(|i| i.light)
            .unwrap_or(0)
    }
}

static AIR_MATERIAL: Material = Material {
    flags: BlockFlags(BlockFlags::AIR.0 | BlockFlags::REPLACEABLE.0),
    shape: Shape::Empty,
    tool: ToolKind::None,
    harvest: 0,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_hash_matches_bedrock() {
        assert_eq!(
            network_block_hash("minecraft:air", Some(&NbtValue::Compound(vec![]))) as i32,
            -604_749_536
        );
    }

    #[test]
    fn hashed_fallback_resolves_air_and_water() {
        let reg = BlockRegistry::fallback(RuntimeIdMode::Hashed);
        let air = reg.air_runtime_id().unwrap();
        assert!(reg.get(air).is_air());
        let water = network_block_hash(
            "minecraft:water",
            Some(&NbtValue::Compound(vec![(
                "liquid_depth".into(),
                NbtValue::Int(0),
            )])),
        );
        assert!(reg.get(water).is_water());
        assert!(!reg.get(12345).is_known());
        assert!(reg.get(12345).is_solid());
    }

    #[test]
    fn sequential_order_is_sorted_by_name_hash() {
        let reg = BlockRegistry::from_states(
            vec![
                ("minecraft:stone".to_string(), None),
                ("minecraft:air".to_string(), None),
                ("minecraft:dirt".to_string(), None),
            ],
            RuntimeIdMode::Sequential,
        );
        let mut names: Vec<_> = (0..3).map(|i| reg.get(i).name().to_string()).collect();
        let mut expected = names.clone();
        expected.sort_by_key(|n| fnv1_64(n.as_bytes()));
        assert_eq!(names, expected);
        names.sort();
        assert_eq!(
            names,
            ["minecraft:air", "minecraft:dirt", "minecraft:stone"]
        );
        let air = reg.air_runtime_id().unwrap();
        assert!(reg.get(air).is_air());
        assert_eq!(reg.runtime_ids_for_name("stone").len(), 1);
    }
}
