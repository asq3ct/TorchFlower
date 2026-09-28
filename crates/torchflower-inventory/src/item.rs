//! Item stacks and the shared item registry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use torchflower_protocol_core::wire::{put_var_i32, put_var_u32, NbtFlavor, WireError, WireReader};

/// Packet id of `ItemRegistry` (1.21.60+).
pub const ITEM_REGISTRY_ID: u32 = 0xa2;

/// An item stack as sent on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemStack {
    pub network_id: i32,
    pub count: u16,
    pub metadata: u32,
    pub block_runtime_id: i32,
    /// Server stack network id (0 when absent).
    pub stack_id: i32,
    /// Opaque "extra data" (NBT, can-place-on, can-destroy); echoed back verbatim.
    pub extra: Box<[u8]>,
}

impl ItemStack {
    /// True for the empty stack.
    pub fn is_empty(&self) -> bool {
        self.network_id == 0 || self.count == 0
    }

    /// Decodes an `ItemInstance` (with optional stack network id).
    pub fn read_instance(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Self::read(r, true)
    }

    /// Decodes an `Item` (without stack network id), as used in recipes.
    pub fn read_plain(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Self::read(r, false)
    }

    fn read(r: &mut WireReader<'_>, with_stack_id: bool) -> Result<Self, WireError> {
        let network_id = r.var_i32()?;
        if network_id == 0 {
            return Ok(Self::default());
        }
        let count = r.u16_le()?;
        let metadata = r.var_u32()?;
        let mut stack_id = 0;
        if with_stack_id && r.bool()? {
            stack_id = r.var_i32()?;
        }
        let block_runtime_id = r.var_i32()?;
        let extra = r.byte_slice()?;
        Ok(Self {
            network_id,
            count,
            metadata,
            block_runtime_id,
            stack_id,
            extra: extra.into(),
        })
    }

    /// Encodes as an `ItemInstance`.
    pub fn write_instance(&self, out: &mut Vec<u8>) {
        put_var_i32(out, self.network_id);
        if self.network_id == 0 {
            return;
        }
        out.extend_from_slice(&self.count.to_le_bytes());
        put_var_u32(out, self.metadata);
        if self.stack_id != 0 {
            out.push(1);
            put_var_i32(out, self.stack_id);
        } else {
            out.push(0);
        }
        put_var_i32(out, self.block_runtime_id);
        if self.extra.is_empty() {
            // Minimal extra data: no NBT, no can-place-on, no can-destroy.
            put_var_u32(out, 10);
            out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        } else {
            put_var_u32(out, self.extra.len() as u32);
            out.extend_from_slice(&self.extra);
        }
    }

    /// Encodes as an `Item` (no stack id).
    pub fn write_plain(&self, out: &mut Vec<u8>) {
        put_var_i32(out, self.network_id);
        if self.network_id == 0 {
            return;
        }
        out.extend_from_slice(&self.count.to_le_bytes());
        put_var_u32(out, self.metadata);
        put_var_i32(out, self.block_runtime_id);
        if self.extra.is_empty() {
            put_var_u32(out, 10);
            out.extend_from_slice(&[0; 10]);
        } else {
            put_var_u32(out, self.extra.len() as u32);
            out.extend_from_slice(&self.extra);
        }
    }

    /// Heap bytes owned. Extra data is kept raw (not parsed into an NBT
    /// tree) to keep slots small.
    pub fn heap_bytes(&self) -> usize {
        self.extra.len()
    }
}

/// One entry of the item registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemEntry {
    pub name: Box<str>,
    pub network_id: i16,
    pub component_based: bool,
}

/// Item name ↔ network id mapping from the `ItemRegistry` packet.
///
/// Registries are de-duplicated process-wide, so bots connected to the same
/// server share one copy.
#[derive(Debug, Default)]
pub struct ItemRegistry {
    by_id: HashMap<i32, u32>,
    by_name: HashMap<Box<str>, u32>,
    entries: Vec<ItemEntry>,
}

fn fnv64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

type SharedCache<T> = Mutex<HashMap<u64, Weak<T>>>;

/// Returns a shared instance for `payload`, decoding it only on cache miss.
pub(crate) fn shared<T>(
    cache: &'static OnceLock<SharedCache<T>>,
    payload: &[u8],
    decode: impl FnOnce(&[u8]) -> Result<T, WireError>,
) -> Result<Arc<T>, WireError> {
    let key = fnv64(payload) ^ (payload.len() as u64).rotate_left(48);
    let cache = cache.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache
        .lock()
        .ok()
        .and_then(|c| c.get(&key).and_then(Weak::upgrade))
    {
        return Ok(hit);
    }
    let value = Arc::new(decode(payload)?);
    if let Ok(mut c) = cache.lock() {
        c.retain(|_, w| w.strong_count() > 0);
        c.insert(key, Arc::downgrade(&value));
    }
    Ok(value)
}

static ITEM_CACHE: OnceLock<SharedCache<ItemRegistry>> = OnceLock::new();

impl ItemRegistry {
    /// Builds a registry from `(name, network_id)` pairs.
    pub fn from_entries(entries: impl IntoIterator<Item = (String, i16)>) -> Self {
        let mut reg = Self::default();
        for (name, id) in entries {
            reg.push(ItemEntry {
                name: name.into_boxed_str(),
                network_id: id,
                component_based: false,
            });
        }
        reg
    }

    fn push(&mut self, entry: ItemEntry) {
        let idx = self.entries.len() as u32;
        self.by_id.insert(entry.network_id as i32, idx);
        self.by_name.insert(entry.name.clone(), idx);
        self.entries.push(entry);
    }

    /// Decodes an `ItemRegistry` packet payload.
    pub fn decode(payload: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        let count = r.var_u32()? as usize;
        let mut reg = Self::default();
        reg.entries.reserve(count.min(8192));
        for _ in 0..count {
            let name = r.string()?.to_string();
            let network_id = r.i16_le()?;
            let component_based = r.bool()?;
            let _version = r.var_i32()?;
            r.skip_nbt(NbtFlavor::Network)?;
            reg.push(ItemEntry {
                name: name.into_boxed_str(),
                network_id,
                component_based,
            });
        }
        Ok(reg)
    }

    /// Decodes with process-wide de-duplication.
    pub fn decode_shared(payload: &[u8]) -> Result<Arc<Self>, WireError> {
        shared(&ITEM_CACHE, payload, Self::decode)
    }

    /// Name for a network id.
    pub fn name(&self, network_id: i32) -> Option<&str> {
        self.by_id
            .get(&network_id)
            .map(|i| &*self.entries[*i as usize].name)
    }

    /// Network id for a name (namespace optional).
    pub fn id(&self, name: &str) -> Option<i32> {
        let found = self.by_name.get(name).or_else(|| {
            if name.contains(':') {
                None
            } else {
                self.by_name.get(format!("minecraft:{name}").as_str())
            }
        });
        found.map(|i| self.entries[*i as usize].network_id as i32)
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torchflower_protocol_core::wire::put_string;

    #[test]
    fn item_instance_round_trip() {
        let item = ItemStack {
            network_id: 5,
            count: 12,
            metadata: 0,
            block_runtime_id: -1234,
            stack_id: 77,
            extra: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0].into(),
        };
        let mut buf = Vec::new();
        item.write_instance(&mut buf);
        let back = ItemStack::read_instance(&mut WireReader::new(&buf)).unwrap();
        assert_eq!(back, item);
        let mut buf = Vec::new();
        ItemStack::default().write_instance(&mut buf);
        assert_eq!(buf, [0]);
    }

    #[test]
    fn item_registry_decode_and_share() {
        let mut p = Vec::new();
        put_var_u32(&mut p, 2);
        for (name, id) in [("minecraft:stone", 1i16), ("minecraft:iron_pickaxe", 330)] {
            put_string(&mut p, name);
            p.extend_from_slice(&id.to_le_bytes());
            p.push(0);
            put_var_i32(&mut p, 1);
            p.extend_from_slice(&[10, 0, 0]); // empty compound
        }
        let a = ItemRegistry::decode_shared(&p).unwrap();
        let b = ItemRegistry::decode_shared(&p).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.id("iron_pickaxe"), Some(330));
        assert_eq!(a.name(1), Some("minecraft:stone"));
    }
}
