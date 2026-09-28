//! Decoders for `LevelChunk` (0x3a), `SubChunk` (0xae) and an encoder for
//! `SubChunkRequest` (0xaf).

use torchflower_protocol_core::wire::{put_var_i32, WireError, WireReader};

use crate::palette::{local_index, PalettedStorage};

/// Packet id of `LevelChunk`.
pub const LEVEL_CHUNK_ID: u32 = 0x3a;
/// Packet id of `SubChunk`.
pub const SUB_CHUNK_ID: u32 = 0xae;
/// Packet id of `SubChunkRequest`.
pub const SUB_CHUNK_REQUEST_ID: u32 = 0xaf;

/// First protocol that carries a render height map in sub-chunk entries (1.21.90).
pub const RENDER_HEIGHTMAP_PROTOCOL: i32 = 818;

/// Decoded sub-chunk: layer 0 (blocks) and optional layer 1 (water-logging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubChunk {
    pub layers: [Option<PalettedStorage>; 2],
}

impl SubChunk {
    /// Uniform sub-chunk.
    pub fn uniform(runtime_id: u32) -> Self {
        Self {
            layers: [Some(PalettedStorage::uniform(runtime_id)), None],
        }
    }

    /// Runtime id at local coordinates on `layer`.
    pub fn get(&self, x: u8, y: u8, z: u8, layer: usize) -> Option<u32> {
        self.layers
            .get(layer)?
            .as_ref()
            .map(|s| s.get(local_index(x, y, z)))
    }

    /// Heap bytes owned.
    pub fn heap_bytes(&self) -> usize {
        self.layers
            .iter()
            .flatten()
            .map(PalettedStorage::heap_bytes)
            .sum()
    }

    /// Decodes a serialized sub-chunk. Returns the absolute sub-chunk Y index
    /// if the payload carries one (version 9).
    ///
    /// Layer 1 is only kept if it contains something other than `air`.
    pub fn decode(
        r: &mut WireReader<'_>,
        air: Option<u32>,
    ) -> Result<(Option<i8>, Self), WireError> {
        let version = r.u8()?;
        let (layer_count, y) = match version {
            1 => (1, None),
            8 => (r.u8()?, None),
            9 => {
                let n = r.u8()?;
                (n, Some(r.i8()?))
            }
            _ => return Err(r.err("sub-chunk version")),
        };
        let mut layers = [None, None];
        for i in 0..layer_count as usize {
            let storage = PalettedStorage::decode(r)?;
            if i == 0 {
                layers[0] = Some(storage);
            } else if i == 1 {
                let only_air = storage.is_uniform() && Some(storage.palette()[0]) == air;
                if !only_air {
                    layers[1] = Some(storage);
                }
            }
        }
        if layers[0].is_none() {
            if let Some(air) = air {
                layers[0] = Some(PalettedStorage::uniform(air));
            }
        }
        Ok((y, Self { layers }))
    }

    /// Encodes as a version-9 sub-chunk (fixtures / tests).
    pub fn encode(&self, y_index: i8, out: &mut Vec<u8>) {
        let count = self.layers.iter().flatten().count() as u8;
        out.push(9);
        out.push(count);
        out.push(y_index as u8);
        for layer in self.layers.iter().flatten() {
            layer.encode(out);
        }
    }
}

/// Sub-chunk delivery mode announced by `LevelChunk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubChunkMode {
    /// Sub-chunks are included inline.
    Inline(u32),
    /// Client must request sub-chunks; no limit.
    RequestLimitless,
    /// Client must request sub-chunks up to (and including) `highest`.
    RequestLimited { highest: u16 },
}

/// `LevelChunk` header plus a borrowed payload.
#[derive(Debug, Clone)]
pub struct LevelChunk<'a> {
    pub chunk_x: i32,
    pub chunk_z: i32,
    pub dimension: i32,
    pub mode: SubChunkMode,
    pub cache_enabled: bool,
    /// Raw payload (sub-chunks, biomes, border blocks, block entities).
    pub payload: &'a [u8],
}

impl<'a> LevelChunk<'a> {
    /// Decodes the packet payload (after the packet header).
    pub fn decode(payload: &'a [u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        let chunk_x = r.var_i32()?;
        let chunk_z = r.var_i32()?;
        let dimension = r.var_i32()?;
        let count = r.var_u32()?;
        let mode = match count {
            u32::MAX => SubChunkMode::RequestLimitless,
            0xffff_fffe => SubChunkMode::RequestLimited {
                highest: r.u16_le()?,
            },
            n => SubChunkMode::Inline(n),
        };
        let cache_enabled = r.bool()?;
        if cache_enabled {
            let blobs = r.var_u32()? as usize;
            r.skip(blobs.saturating_mul(8), "blob hashes")?;
        }
        let payload = r.byte_slice()?;
        Ok(Self {
            chunk_x,
            chunk_z,
            dimension,
            mode,
            cache_enabled,
            payload,
        })
    }

    /// Iterates over inline sub-chunks as `(absolute_y_index, SubChunk)`.
    ///
    /// `min_sub_y` is the dimension's lowest sub-chunk index (−4 in the
    /// overworld). Sub-chunks are not length-prefixed, so every entry must be
    /// parsed, but only those for which `want(y)` returns true are handed to
    /// `sink`; the rest are dropped immediately.
    pub fn for_each_sub_chunk(
        &self,
        min_sub_y: i8,
        air: Option<u32>,
        mut want: impl FnMut(i8) -> bool,
        mut sink: impl FnMut(i8, SubChunk),
    ) -> Result<u32, WireError> {
        let SubChunkMode::Inline(count) = self.mode else {
            return Ok(0);
        };
        if self.cache_enabled {
            return Ok(0);
        }
        let mut r = WireReader::new(self.payload);
        let mut kept = 0;
        for i in 0..count.min(64) {
            let (y, sub) = SubChunk::decode(&mut r, air)?;
            let y = y.unwrap_or(min_sub_y.saturating_add(i as i8));
            if want(y) {
                sink(y, sub);
                kept += 1;
            }
        }
        Ok(kept)
    }
}

/// Result code of a sub-chunk entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubChunkResult {
    Undefined,
    Success,
    ChunkNotFound,
    InvalidDimension,
    PlayerNotFound,
    IndexOutOfBounds,
    SuccessAllAir,
}

impl From<u8> for SubChunkResult {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Success,
            2 => Self::ChunkNotFound,
            3 => Self::InvalidDimension,
            4 => Self::PlayerNotFound,
            5 => Self::IndexOutOfBounds,
            6 => Self::SuccessAllAir,
            _ => Self::Undefined,
        }
    }
}

/// Decoded `SubChunk` entry in absolute sub-chunk coordinates.
#[derive(Debug, Clone)]
pub struct SubChunkEntry<'a> {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub result: SubChunkResult,
    pub payload: &'a [u8],
}

/// Decodes a `SubChunk` packet (protocol < 1.26.40 layout) and calls `sink`
/// for every entry. Returns the dimension.
pub fn decode_sub_chunk_packet<'a>(
    payload: &'a [u8],
    protocol: i32,
    mut sink: impl FnMut(SubChunkEntry<'a>),
) -> Result<i32, WireError> {
    let mut r = WireReader::new(payload);
    let cache = r.bool()?;
    let dimension = r.var_i32()?;
    let base = r.block_pos()?;
    let count = r.u32_le()?;
    for _ in 0..count.min(4096) {
        let dx = r.i8()? as i32;
        let dy = r.i8()? as i32;
        let dz = r.i8()? as i32;
        let result = SubChunkResult::from(r.u8()?);
        let data = if !cache || result != SubChunkResult::SuccessAllAir {
            r.byte_slice()?
        } else {
            &[][..]
        };
        if r.u8()? == 1 {
            r.skip(256, "height map")?;
        }
        if protocol >= RENDER_HEIGHTMAP_PROTOCOL && r.u8()? == 1 {
            r.skip(256, "render height map")?;
        }
        if cache {
            r.u64_le()?;
        }
        sink(SubChunkEntry {
            x: base[0] + dx,
            y: base[1] + dy,
            z: base[2] + dz,
            result,
            payload: data,
        });
    }
    Ok(dimension)
}

/// Appends a framed `SubChunkRequest` asking for the given absolute
/// sub-chunk positions (all within ±127 of `base`).
pub fn encode_sub_chunk_request(
    out: &mut Vec<u8>,
    dimension: i32,
    base: [i32; 3],
    positions: &[[i32; 3]],
) {
    use torchflower_protocol_core::wire::{begin_packet, end_packet, put_block_pos};
    let mark = begin_packet(out, SUB_CHUNK_REQUEST_ID);
    put_var_i32(out, dimension);
    put_block_pos(out, base);
    let valid: Vec<[i8; 3]> = positions
        .iter()
        .filter_map(|p| {
            let d = [p[0] - base[0], p[1] - base[1], p[2] - base[2]];
            d.iter()
                .all(|v| (-128..=127).contains(v))
                .then(|| [d[0] as i8, d[1] as i8, d[2] as i8])
        })
        .collect();
    out.extend_from_slice(&(valid.len() as u32).to_le_bytes());
    for d in valid {
        out.extend_from_slice(&[d[0] as u8, d[1] as u8, d[2] as u8]);
    }
    end_packet(out, mark);
}

/// Lowest sub-chunk index for a dimension id (0 overworld, 1 nether, 2 end).
pub fn min_sub_chunk_y(dimension: i32) -> i8 {
    if dimension == 0 {
        -4
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torchflower_protocol_core::wire::{iter_packets, put_var_u32};

    #[test]
    fn level_chunk_inline_round_trip() {
        let mut sub = SubChunk::uniform(1);
        sub.layers[0].as_mut().unwrap().set(local_index(1, 2, 3), 5);
        let mut body = Vec::new();
        sub.encode(-4, &mut body);
        SubChunk::uniform(1).encode(-3, &mut body);

        let mut pkt = Vec::new();
        put_var_i32(&mut pkt, 3);
        put_var_i32(&mut pkt, -2);
        put_var_i32(&mut pkt, 0);
        put_var_u32(&mut pkt, 2);
        pkt.push(0);
        put_var_u32(&mut pkt, body.len() as u32);
        pkt.extend_from_slice(&body);

        let chunk = LevelChunk::decode(&pkt).unwrap();
        assert_eq!((chunk.chunk_x, chunk.chunk_z), (3, -2));
        let mut got = Vec::new();
        let kept = chunk
            .for_each_sub_chunk(-4, Some(1), |y| y == -4, |y, s| got.push((y, s)))
            .unwrap();
        assert_eq!(kept, 1);
        assert_eq!(got[0].1.get(1, 2, 3, 0), Some(5));
    }

    #[test]
    fn request_mode_detected() {
        let mut pkt = Vec::new();
        put_var_i32(&mut pkt, 0);
        put_var_i32(&mut pkt, 0);
        put_var_i32(&mut pkt, 0);
        put_var_u32(&mut pkt, 0xffff_fffe);
        pkt.extend_from_slice(&7u16.to_le_bytes());
        pkt.push(0);
        put_var_u32(&mut pkt, 0);
        let chunk = LevelChunk::decode(&pkt).unwrap();
        assert_eq!(chunk.mode, SubChunkMode::RequestLimited { highest: 7 });
    }

    #[test]
    fn sub_chunk_packet_decode() {
        let mut sub_bytes = Vec::new();
        SubChunk::uniform(9).encode(2, &mut sub_bytes);
        let mut pkt = Vec::new();
        pkt.push(0); // no cache
        put_var_i32(&mut pkt, 0);
        torchflower_protocol_core::wire::put_block_pos(&mut pkt, [4, 2, -1]);
        pkt.extend_from_slice(&1u32.to_le_bytes());
        pkt.extend_from_slice(&[1u8, 0, 0xff]);
        pkt.push(1);
        put_var_u32(&mut pkt, sub_bytes.len() as u32);
        pkt.extend_from_slice(&sub_bytes);
        pkt.push(0); // heightmap none
        pkt.push(0); // render heightmap none
        let mut entries = Vec::new();
        decode_sub_chunk_packet(&pkt, 898, |e| {
            entries.push((e.x, e.y, e.z, e.result, e.payload.len()))
        })
        .unwrap();
        assert_eq!(
            entries,
            vec![(5, 2, -2, SubChunkResult::Success, sub_bytes.len())]
        );
    }

    #[test]
    fn sub_chunk_request_encoding() {
        let mut out = Vec::new();
        encode_sub_chunk_request(
            &mut out,
            0,
            [0, 4, 0],
            &[[1, 4, 0], [0, 3, -1], [500, 0, 0]],
        );
        let pkt = iter_packets(&out).next().unwrap().unwrap();
        assert_eq!(pkt.id, SUB_CHUNK_REQUEST_ID);
        let mut r = WireReader::new(pkt.payload);
        assert_eq!(r.var_i32().unwrap(), 0);
        assert_eq!(r.block_pos().unwrap(), [0, 4, 0]);
        assert_eq!(r.u32_le().unwrap(), 2);
    }
}
