//! Embedded vanilla block palettes (no external files needed).
//!
//! `tools/gen_palettes.py` converts pmmp/BedrockData's
//! `canonical_block_states.nbt` (CC0-1.0) for every supported game version
//! into a compact form, stored zlib-compressed in `data/palettes/`. Each block
//! lists its states as a full cartesian product of its property values, so
//! the generator stores only names, property value lists and the enumeration
//! order: about 14 KB per version instead of more than 2 MB of NBT.
//!
//! # Compact format (after zlib inflation)
//!
//! All integers are LEB128 varuints unless noted.
//!
//! ```text
//! "TFBP" u8:format(1) state_version
//! strings: count, then (byte_len, utf8)...
//! keys:    count, then (string_index, u8:nbt_tag)...
//! blocks:  count, then per block:
//!     name_index, prop_count,
//!     per prop: key_index, value_count, values
//!         (tag 1 byte: u8; tag 3 int: zig-zag varint; tag 8 string: string_index)
//!     prop_count bytes: significance order (most significant first)
//! ```
//!
//! States are expanded in canonical order: for each block, the cartesian
//! product of the property values, where the property listed first in the
//! significance order changes slowest.

use std::io::Read;

use torchflower_protocol_core::wire::NbtValue;

use crate::palette_index::PALETTES;

/// Error decoding an embedded palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteError(pub &'static str);

impl std::fmt::Display for PaletteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid embedded palette: {}", self.0)
    }
}

impl std::error::Error for PaletteError {}

/// Metadata of one embedded palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedPalette {
    /// Lowest protocol version this palette applies to.
    pub first_protocol: i32,
    /// BedrockData release tag the palette was generated from.
    pub source_tag: &'static str,
    /// Number of block states.
    pub state_count: usize,
    data: &'static [u8],
}

impl EmbeddedPalette {
    /// Expands the palette into `(name, states)` pairs in canonical order.
    pub fn states(&self) -> Result<Vec<(String, Option<NbtValue>)>, PaletteError> {
        let mut raw = Vec::with_capacity(64 * 1024);
        flate2::read::ZlibDecoder::new(self.data)
            .read_to_end(&mut raw)
            .map_err(|_| PaletteError("zlib stream"))?;
        let states = expand(&raw)?;
        if states.len() != self.state_count {
            return Err(PaletteError("state count mismatch"));
        }
        Ok(states)
    }
}

/// All embedded palettes, ordered by protocol.
pub fn embedded_palettes() -> impl Iterator<Item = EmbeddedPalette> {
    PALETTES.iter().map(
        |&(first_protocol, source_tag, data, state_count)| EmbeddedPalette {
            first_protocol,
            source_tag,
            state_count,
            data,
        },
    )
}

/// Palette for `protocol`: the newest palette whose first protocol is at or
/// below `protocol`. Protocols older than every embedded palette get the
/// oldest one. Returns `None` only if no palettes are embedded.
pub fn embedded_palette_for(protocol: i32) -> Option<EmbeddedPalette> {
    let mut chosen = None;
    for p in embedded_palettes() {
        if chosen.is_none() || p.first_protocol <= protocol {
            chosen = Some(p);
        }
    }
    chosen
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn u8(&mut self) -> Result<u8, PaletteError> {
        let b = *self.buf.get(self.pos).ok_or(PaletteError("truncated"))?;
        self.pos += 1;
        Ok(b)
    }

    fn var(&mut self) -> Result<u32, PaletteError> {
        let mut v = 0u32;
        for shift in (0..35).step_by(7) {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u32) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(PaletteError("varint overflow"))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], PaletteError> {
        let end = self.pos.checked_add(n).ok_or(PaletteError("length"))?;
        let out = self
            .buf
            .get(self.pos..end)
            .ok_or(PaletteError("truncated"))?;
        self.pos = end;
        Ok(out)
    }
}

enum Value {
    Byte(i8),
    Int(i32),
    Str(u32),
}

fn expand(raw: &[u8]) -> Result<Vec<(String, Option<NbtValue>)>, PaletteError> {
    let mut c = Cursor { buf: raw, pos: 0 };
    if c.bytes(4)? != b"TFBP" {
        return Err(PaletteError("magic"));
    }
    if c.u8()? != 1 {
        return Err(PaletteError("format version"));
    }
    let _state_version = c.var()?;
    let n_strings = c.var()? as usize;
    let mut strings = Vec::with_capacity(n_strings.min(8192));
    for _ in 0..n_strings {
        let len = c.var()? as usize;
        let s = std::str::from_utf8(c.bytes(len)?).map_err(|_| PaletteError("utf-8"))?;
        strings.push(s.to_string());
    }
    let string = |i: u32| -> Result<&String, PaletteError> {
        strings.get(i as usize).ok_or(PaletteError("string index"))
    };
    let n_keys = c.var()? as usize;
    let mut keys = Vec::with_capacity(n_keys.min(1024));
    for _ in 0..n_keys {
        let name = c.var()?;
        let tag = c.u8()?;
        if !matches!(tag, 1 | 3 | 8) {
            return Err(PaletteError("property tag"));
        }
        keys.push((name, tag));
    }
    let n_blocks = c.var()? as usize;
    let mut out = Vec::with_capacity(20_000);
    for _ in 0..n_blocks {
        let name = string(c.var()?)?.clone();
        let n_props = c.var()? as usize;
        if n_props > 16 {
            return Err(PaletteError("too many properties"));
        }
        let mut props: Vec<(u32, u8, Vec<Value>)> = Vec::with_capacity(n_props);
        for _ in 0..n_props {
            let (key, tag) = *keys
                .get(c.var()? as usize)
                .ok_or(PaletteError("key index"))?;
            let n_vals = c.var()? as usize;
            if n_vals == 0 || n_vals > 256 {
                return Err(PaletteError("value count"));
            }
            let mut vals = Vec::with_capacity(n_vals);
            for _ in 0..n_vals {
                vals.push(match tag {
                    1 => Value::Byte(c.u8()? as i8),
                    3 => {
                        let z = c.var()?;
                        Value::Int(((z >> 1) as i32) ^ -((z & 1) as i32))
                    }
                    _ => Value::Str(c.var()?),
                });
            }
            props.push((key, tag, vals));
        }
        let order = c.bytes(n_props)?.to_vec();
        if order.iter().any(|&o| o as usize >= n_props) {
            return Err(PaletteError("order index"));
        }
        let total: usize = props.iter().map(|p| p.2.len()).product();
        if total > 65_536 {
            return Err(PaletteError("block has too many states"));
        }
        let mut digits = vec![0usize; n_props];
        for _ in 0..total {
            let mut entries = Vec::with_capacity(n_props);
            for (i, (key, _tag, vals)) in props.iter().enumerate() {
                let value = match &vals[digits[i]] {
                    Value::Byte(b) => NbtValue::Byte(*b),
                    Value::Int(v) => NbtValue::Int(*v),
                    Value::Str(s) => NbtValue::String(string(*s)?.clone()),
                };
                entries.push((string(*key)?.clone(), value));
            }
            out.push((name.clone(), Some(NbtValue::Compound(entries))));
            // Increment the mixed-radix counter: least significant first.
            for &p in order.iter().rev() {
                let p = p as usize;
                digits[p] += 1;
                if digits[p] < props[p].2.len() {
                    break;
                }
                digits[p] = 0;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::network_block_hash;

    #[test]
    fn every_embedded_palette_expands() {
        let mut last = 0;
        for p in embedded_palettes() {
            assert!(p.first_protocol > last, "palettes must be sorted");
            last = p.first_protocol;
            let states = p.states().unwrap();
            assert_eq!(states.len(), p.state_count, "{}", p.source_tag);
            assert!(states.iter().any(|(n, _)| n == "minecraft:air"));
        }
    }

    #[test]
    fn selection_by_protocol() {
        // The release each protocol belongs to (game protocol numbers).
        let cases = [
            (1, "bedrock-1.21.50"),
            (766, "bedrock-1.21.50"),
            (776, "bedrock-1.21.60"),
            (786, "bedrock-1.21.70"),
            (800, "bedrock-1.21.80"),
            (818, "bedrock-1.21.90"),
            (819, "bedrock-1.21.93"),
            (827, "6.0.0+bedrock-1.21.100"),
            (844, "bedrock-1.21.111"),
            (859, "bedrock-1.21.120"),
            (860, "bedrock-1.21.120"),
            (897, "bedrock-1.21.130"),
            (898, "bedrock-1.21.130"),
            (924, "bedrock-1.26.0"),
            (944, "bedrock-1.26.10"),
            (975, "bedrock-1.26.20"),
            (990, "bedrock-1.26.20"),
            (1001, "bedrock-1.26.30"),
            (5000, "bedrock-1.26.30"),
        ];
        for (protocol, tag) in cases {
            assert_eq!(
                embedded_palette_for(protocol).unwrap().source_tag,
                tag,
                "protocol {protocol}"
            );
        }
    }

    /// Every distinct embedded palette reproduces its BedrockData NBT file:
    /// same state count and the same network hash for every state, in order.
    #[test]
    fn every_palette_matches_bedrockdata() {
        let mut checked = std::collections::HashSet::new();
        for p in embedded_palettes() {
            if !checked.insert(p.data.as_ptr() as usize) {
                continue; // shared with an earlier release
            }
            let (_, count, digest) = REFERENCE_ALL_PALETTES
                .iter()
                .find(|r| r.0 == p.source_tag)
                .unwrap_or_else(|| panic!("no reference for {}", p.source_tag));
            let states = p.states().unwrap();
            assert_eq!(states.len(), *count, "{}", p.source_tag);
            let mut all: u32 = 0x811c_9dc5;
            for (name, st) in &states {
                for b in network_block_hash(name, st.as_ref()).to_le_bytes() {
                    all ^= b as u32;
                    all = all.wrapping_mul(0x0100_0193);
                }
            }
            assert_eq!(all, *digest, "state hashes of {}", p.source_tag);
        }
        assert_eq!(checked.len(), REFERENCE_ALL_PALETTES.len());
    }

    /// Canonical order and hashes match the original NBT file. Reference
    /// values were computed from BedrockData's `canonical_block_states.nbt`
    /// for 1.21.130 with an independent script.
    #[test]
    fn matches_bedrockdata_1_21_130() {
        let states = embedded_palette_for(898).unwrap().states().unwrap();
        let hash = |i: usize| network_block_hash(&states[i].0, states[i].1.as_ref());
        for (index, name, expected) in REFERENCE_1_21_130 {
            assert_eq!(states[*index].0, *name, "state {index}");
            assert_eq!(hash(*index), *expected, "hash of state {index} ({name})");
        }
        let mut all: u32 = 0x811c_9dc5;
        for i in 0..states.len() {
            for b in hash(i).to_le_bytes() {
                all ^= b as u32;
                all = all.wrapping_mul(0x0100_0193);
            }
        }
        assert_eq!(all, REFERENCE_1_21_130_ALL_HASHES);
    }

    include!("palette_reference.rs");
}
