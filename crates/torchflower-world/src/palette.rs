//! Compact paletted block storage for one 16×16×16 sub-chunk layer.

use torchflower_protocol_core::wire::{WireError, WireReader};

const VALID_BITS: [u8; 8] = [1, 2, 3, 4, 5, 6, 8, 16];

fn word_count(bits: u8) -> usize {
    if bits == 0 {
        return 0;
    }
    let per_word = 32 / bits as usize;
    4096_usize.div_ceil(per_word)
}

fn bits_for(len: usize) -> u8 {
    if len <= 1 {
        return 0;
    }
    let needed = usize::BITS - (len - 1).leading_zeros();
    VALID_BITS
        .iter()
        .copied()
        .find(|b| *b as u32 >= needed)
        .unwrap_or(16)
}

/// Index of a block inside a sub-chunk (Bedrock XZY order).
#[inline]
pub fn local_index(x: u8, y: u8, z: u8) -> usize {
    ((x as usize & 15) << 8) | ((z as usize & 15) << 4) | (y as usize & 15)
}

/// Bit-packed palette storage. Uniform layers use no index words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PalettedStorage {
    bits: u8,
    words: Box<[u32]>,
    palette: Box<[u32]>,
}

impl PalettedStorage {
    /// Uniform storage filled with `runtime_id`.
    pub fn uniform(runtime_id: u32) -> Self {
        Self {
            bits: 0,
            words: Box::new([]),
            palette: Box::new([runtime_id]),
        }
    }

    /// Decodes one network-encoded storage layer (runtime-id palette).
    ///
    /// The result is re-packed to the smallest bit width able to hold the
    /// palette, which frequently halves memory compared to the wire form.
    pub fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let header = r.u8()?;
        let bits = header >> 1;
        if header & 1 == 0 {
            return Err(r.err("persistent (NBT) sub-chunk palette"));
        }
        if bits != 0 && !VALID_BITS.contains(&bits) {
            return Err(r.err("sub-chunk bits per block"));
        }
        let n_words = word_count(bits);
        let raw_words = r.bytes(n_words * 4, "sub-chunk words")?;
        let palette_len = if bits == 0 {
            1
        } else {
            let len = r.var_i32()?;
            if len <= 0 || len > 4096 {
                return Err(r.err("sub-chunk palette length"));
            }
            len as usize
        };
        let mut palette = Vec::with_capacity(palette_len);
        for _ in 0..palette_len {
            palette.push(r.var_i32()? as u32);
        }
        if bits == 0 {
            return Ok(Self {
                bits: 0,
                words: Box::new([]),
                palette: palette.into_boxed_slice(),
            });
        }
        let words: Vec<u32> = raw_words
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let decoded = Self {
            bits,
            words: words.into_boxed_slice(),
            palette: palette.into_boxed_slice(),
        };
        Ok(decoded.compacted())
    }

    /// Bits per block index.
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// Palette entries (runtime ids).
    pub fn palette(&self) -> &[u32] {
        &self.palette
    }

    /// True if every block is the same state.
    pub fn is_uniform(&self) -> bool {
        self.palette.len() == 1 || self.bits == 0
    }

    #[inline]
    fn index_at(&self, i: usize) -> usize {
        if self.bits == 0 {
            return 0;
        }
        let per_word = 32 / self.bits as usize;
        let word = self.words[i / per_word];
        let shift = (i % per_word) * self.bits as usize;
        ((word >> shift) & ((1u32 << self.bits) - 1)) as usize
    }

    #[inline]
    fn set_index(&mut self, i: usize, value: usize) {
        let per_word = 32 / self.bits as usize;
        let shift = (i % per_word) * self.bits as usize;
        let mask = ((1u32 << self.bits) - 1) << shift;
        let w = &mut self.words[i / per_word];
        *w = (*w & !mask) | (((value as u32) << shift) & mask);
    }

    /// Runtime id at a local index.
    #[inline]
    pub fn get(&self, i: usize) -> u32 {
        let idx = self.index_at(i);
        self.palette.get(idx).copied().unwrap_or(self.palette[0])
    }

    /// Sets the runtime id at a local index, growing the palette if needed.
    pub fn set(&mut self, i: usize, runtime_id: u32) {
        let pos = match self.palette.iter().position(|p| *p == runtime_id) {
            Some(p) => p,
            None => {
                let mut palette = self.palette.to_vec();
                palette.push(runtime_id);
                let needed = bits_for(palette.len());
                if needed > self.bits {
                    self.repack(needed);
                }
                self.palette = palette.into_boxed_slice();
                self.palette.len() - 1
            }
        };
        if self.bits == 0 {
            if pos == 0 {
                return;
            }
            self.repack(bits_for(self.palette.len()));
        }
        self.set_index(i, pos);
    }

    fn repack(&mut self, bits: u8) {
        let mut next = PalettedStorage {
            bits,
            words: vec![0u32; word_count(bits)].into_boxed_slice(),
            palette: self.palette.clone(),
        };
        if bits > 0 {
            for i in 0..4096 {
                next.set_index(i, self.index_at(i));
            }
        }
        self.bits = next.bits;
        self.words = next.words;
    }

    /// Drops unused palette entries and re-packs to the minimal width.
    pub fn compacted(self) -> Self {
        if self.bits == 0 {
            return self;
        }
        let mut used = vec![false; self.palette.len()];
        for i in 0..4096 {
            if let Some(u) = used.get_mut(self.index_at(i)) {
                *u = true;
            }
        }
        let mut remap = vec![0usize; self.palette.len()];
        let mut palette = Vec::with_capacity(self.palette.len());
        for (old, keep) in used.iter().enumerate() {
            if *keep {
                remap[old] = palette.len();
                palette.push(self.palette[old]);
            }
        }
        if palette.is_empty() {
            palette.push(self.palette[0]);
        }
        let bits = bits_for(palette.len());
        if bits == 0 {
            return Self::uniform(palette[0]);
        }
        if bits == self.bits && palette.len() == self.palette.len() {
            return self;
        }
        let mut next = PalettedStorage {
            bits,
            words: vec![0u32; word_count(bits)].into_boxed_slice(),
            palette: palette.into_boxed_slice(),
        };
        for i in 0..4096 {
            let old = self.index_at(i);
            next.set_index(i, remap.get(old).copied().unwrap_or(0));
        }
        next
    }

    /// Heap bytes owned by this storage.
    pub fn heap_bytes(&self) -> usize {
        self.words.len() * 4 + self.palette.len() * 4
    }

    /// Encodes this layer in network form (used by tests and fixtures).
    pub fn encode(&self, out: &mut Vec<u8>) {
        use torchflower_protocol_core::wire::put_var_i32;
        out.push((self.bits << 1) | 1);
        for w in self.words.iter() {
            out.extend_from_slice(&w.to_le_bytes());
        }
        if self.bits != 0 {
            put_var_i32(out, self.palette.len() as i32);
        }
        for p in self.palette.iter() {
            put_var_i32(out, *p as i32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_and_growth() {
        let mut s = PalettedStorage::uniform(7);
        assert_eq!(s.get(local_index(3, 4, 5)), 7);
        s.set(local_index(3, 4, 5), 9);
        assert_eq!(s.bits(), 1);
        assert_eq!(s.get(local_index(3, 4, 5)), 9);
        for id in 10..40 {
            s.set(id as usize, id);
        }
        assert_eq!(s.bits(), 5);
        for id in 10..40u32 {
            assert_eq!(s.get(id as usize), id);
        }
        assert_eq!(s.get(local_index(3, 4, 5)), 9);
        assert_eq!(s.get(4095), 7);
    }

    #[test]
    fn decode_repacks_to_minimal_width() {
        // Encode a 16-bit wide storage that only uses two states.
        let mut wide = PalettedStorage {
            bits: 16,
            words: vec![0u32; word_count(16)].into_boxed_slice(),
            palette: vec![1, 2, 3, 4].into_boxed_slice(),
        };
        wide.set_index(10, 1);
        let mut buf = Vec::new();
        wide.encode(&mut buf);
        let decoded = PalettedStorage::decode(&mut WireReader::new(&buf)).unwrap();
        assert_eq!(decoded.bits(), 1);
        assert_eq!(decoded.palette(), &[1, 2]);
        assert_eq!(decoded.get(10), 2);
        assert_eq!(decoded.get(11), 1);
        assert!(decoded.heap_bytes() < wide.heap_bytes() / 8);
    }

    #[test]
    fn three_bit_padding_layout() {
        assert_eq!(word_count(3), 410);
        assert_eq!(word_count(5), 683);
        assert_eq!(word_count(6), 820);
    }
}
