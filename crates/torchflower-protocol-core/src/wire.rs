//! Slice-based, allocation-free wire codec helpers for Bedrock game packets.
//!
//! [`WireReader`] borrows the packet buffer and hands out sub-slices instead of
//! copying, which keeps the per-tick decode path free of heap allocations. The
//! `put_*` functions append to a caller-owned (and typically pooled) `Vec<u8>`.
//!
//! The module also contains a small NBT implementation that understands both
//! the network (varint) and the plain little-endian flavour of Bedrock NBT.

use std::fmt;

/// Error produced by [`WireReader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    /// Field that failed to decode.
    pub what: &'static str,
    /// Byte offset at which decoding failed.
    pub offset: usize,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to decode {} at offset {}",
            self.what, self.offset
        )
    }
}

impl std::error::Error for WireError {}

/// Result alias for wire decoding.
pub type WireResult<T> = Result<T, WireError>;

/// Zero-copy cursor over a byte slice.
#[derive(Debug, Clone)]
pub struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

macro_rules! read_le {
    ($name:ident, $ty:ty, $len:expr) => {
        #[doc = concat!("Reads a little-endian `", stringify!($ty), "`.")]
        pub fn $name(&mut self) -> WireResult<$ty> {
            let bytes = self.bytes($len, stringify!($ty))?;
            let mut arr = [0u8; $len];
            arr.copy_from_slice(bytes);
            Ok(<$ty>::from_le_bytes(arr))
        }
    };
}

impl<'a> WireReader<'a> {
    /// Creates a reader positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Current byte offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Number of unread bytes.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Returns the unread tail without consuming it.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }

    /// Builds an error at the current position.
    pub fn err(&self, what: &'static str) -> WireError {
        WireError {
            what,
            offset: self.pos,
        }
    }

    /// Borrows the next `len` bytes.
    pub fn bytes(&mut self, len: usize, what: &'static str) -> WireResult<&'a [u8]> {
        let end = self.pos.checked_add(len).ok_or_else(|| self.err(what))?;
        let out = self.buf.get(self.pos..end).ok_or_else(|| self.err(what))?;
        self.pos = end;
        Ok(out)
    }

    /// Skips `len` bytes.
    pub fn skip(&mut self, len: usize, what: &'static str) -> WireResult<()> {
        self.bytes(len, what).map(|_| ())
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> WireResult<u8> {
        let b = *self.buf.get(self.pos).ok_or_else(|| self.err("u8"))?;
        self.pos += 1;
        Ok(b)
    }

    /// Reads one signed byte.
    pub fn i8(&mut self) -> WireResult<i8> {
        self.u8().map(|b| b as i8)
    }

    /// Reads a boolean byte.
    pub fn bool(&mut self) -> WireResult<bool> {
        self.u8().map(|b| b != 0)
    }

    read_le!(u16_le, u16, 2);
    read_le!(i16_le, i16, 2);
    read_le!(u32_le, u32, 4);
    read_le!(i32_le, i32, 4);
    read_le!(u64_le, u64, 8);
    read_le!(i64_le, i64, 8);
    read_le!(f32_le, f32, 4);
    read_le!(f64_le, f64, 8);

    /// Reads an unsigned LEB128 varint (max 32 bits).
    pub fn var_u32(&mut self) -> WireResult<u32> {
        let mut value = 0u32;
        for i in 0..5 {
            let b = self.u8().map_err(|_| self.err("varuint32"))?;
            value |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(self.err("varuint32 overflow"))
    }

    /// Reads a zig-zag encoded signed varint (Bedrock `varint32`).
    pub fn var_i32(&mut self) -> WireResult<i32> {
        let v = self.var_u32()?;
        Ok(((v >> 1) as i32) ^ -((v & 1) as i32))
    }

    /// Reads an unsigned LEB128 varint (max 64 bits).
    pub fn var_u64(&mut self) -> WireResult<u64> {
        let mut value = 0u64;
        for i in 0..10 {
            let b = self.u8().map_err(|_| self.err("varuint64"))?;
            value |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(self.err("varuint64 overflow"))
    }

    /// Reads a zig-zag encoded signed 64-bit varint.
    pub fn var_i64(&mut self) -> WireResult<i64> {
        let v = self.var_u64()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    /// Reads a varuint32-prefixed byte slice without copying.
    pub fn byte_slice(&mut self) -> WireResult<&'a [u8]> {
        let len = self.var_u32()? as usize;
        self.bytes(len, "byte slice")
    }

    /// Reads a varuint32-prefixed UTF-8 string without copying.
    pub fn string(&mut self) -> WireResult<&'a str> {
        let at = self.pos;
        let raw = self.byte_slice()?;
        std::str::from_utf8(raw).map_err(|_| WireError {
            what: "utf-8 string",
            offset: at,
        })
    }

    /// Reads three little-endian floats.
    pub fn vec3(&mut self) -> WireResult<[f32; 3]> {
        Ok([self.f32_le()?, self.f32_le()?, self.f32_le()?])
    }

    /// Reads two little-endian floats.
    pub fn vec2(&mut self) -> WireResult<[f32; 2]> {
        Ok([self.f32_le()?, self.f32_le()?])
    }

    /// Reads a block position encoded as three zig-zag varints.
    pub fn block_pos(&mut self) -> WireResult<[i32; 3]> {
        Ok([self.var_i32()?, self.var_i32()?, self.var_i32()?])
    }

    /// Reads a block position whose Y component is an unsigned varint.
    pub fn ublock_pos(&mut self) -> WireResult<[i32; 3]> {
        let x = self.var_i32()?;
        let y = self.var_u32()? as i32;
        let z = self.var_i32()?;
        Ok([x, y, z])
    }

    /// Reads a rotation byte (`value * 360 / 256`).
    pub fn byte_angle(&mut self) -> WireResult<f32> {
        self.u8().map(|v| v as f32 * (360.0 / 256.0))
    }

    /// Skips a single NBT root tag (name included) of the given flavour.
    pub fn skip_nbt(&mut self, flavor: NbtFlavor) -> WireResult<()> {
        let tag = self.u8()?;
        if tag == 0 {
            return Ok(());
        }
        self.skip_nbt_string(flavor)?;
        self.skip_nbt_payload(tag, flavor, 0)
    }

    fn nbt_len(&mut self, flavor: NbtFlavor) -> WireResult<usize> {
        let len = match flavor {
            NbtFlavor::Network => self.var_i32()?,
            NbtFlavor::LittleEndian => self.i32_le()?,
        };
        usize::try_from(len).map_err(|_| self.err("negative NBT length"))
    }

    fn skip_nbt_string(&mut self, flavor: NbtFlavor) -> WireResult<()> {
        let len = match flavor {
            NbtFlavor::Network => self.var_u32()? as usize,
            NbtFlavor::LittleEndian => self.u16_le()? as usize,
        };
        self.skip(len, "NBT string")
    }

    fn read_nbt_string(&mut self, flavor: NbtFlavor) -> WireResult<String> {
        let len = match flavor {
            NbtFlavor::Network => self.var_u32()? as usize,
            NbtFlavor::LittleEndian => self.u16_le()? as usize,
        };
        let raw = self.bytes(len, "NBT string")?;
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    fn skip_nbt_payload(&mut self, tag: u8, flavor: NbtFlavor, depth: u32) -> WireResult<()> {
        if depth > NBT_MAX_DEPTH {
            return Err(self.err("NBT nesting too deep"));
        }
        let net = flavor == NbtFlavor::Network;
        match tag {
            1 => self.skip(1, "NBT byte"),
            2 => self.skip(2, "NBT short"),
            3 if net => self.var_i32().map(|_| ()),
            3 => self.skip(4, "NBT int"),
            4 if net => self.var_i64().map(|_| ()),
            4 => self.skip(8, "NBT long"),
            5 => self.skip(4, "NBT float"),
            6 => self.skip(8, "NBT double"),
            7 => {
                let len = self.nbt_len(flavor)?;
                self.skip(len, "NBT byte array")
            }
            8 => self.skip_nbt_string(flavor),
            9 => {
                let inner = self.u8()?;
                let len = self.nbt_len(flavor)?;
                for _ in 0..len {
                    self.skip_nbt_payload(inner, flavor, depth + 1)?;
                }
                Ok(())
            }
            10 => loop {
                let inner = self.u8()?;
                if inner == 0 {
                    return Ok(());
                }
                self.skip_nbt_string(flavor)?;
                self.skip_nbt_payload(inner, flavor, depth + 1)?;
            },
            11 => {
                let len = self.nbt_len(flavor)?;
                for _ in 0..len {
                    if net {
                        self.var_i32()?;
                    } else {
                        self.skip(4, "NBT int array")?;
                    }
                }
                Ok(())
            }
            12 => {
                let len = self.nbt_len(flavor)?;
                for _ in 0..len {
                    if net {
                        self.var_i64()?;
                    } else {
                        self.skip(8, "NBT long array")?;
                    }
                }
                Ok(())
            }
            _ => Err(self.err("unknown NBT tag")),
        }
    }

    /// Reads a named NBT root tag, returning `(name, value)`.
    pub fn read_nbt(&mut self, flavor: NbtFlavor) -> WireResult<(String, NbtValue)> {
        let tag = self.u8()?;
        if tag == 0 {
            return Ok((String::new(), NbtValue::End));
        }
        let name = self.read_nbt_string(flavor)?;
        let value = self.read_nbt_payload(tag, flavor, 0)?;
        Ok((name, value))
    }

    fn read_nbt_payload(&mut self, tag: u8, flavor: NbtFlavor, depth: u32) -> WireResult<NbtValue> {
        if depth > NBT_MAX_DEPTH {
            return Err(self.err("NBT nesting too deep"));
        }
        let net = flavor == NbtFlavor::Network;
        Ok(match tag {
            1 => NbtValue::Byte(self.i8()?),
            2 => NbtValue::Short(self.i16_le()?),
            3 => NbtValue::Int(if net { self.var_i32()? } else { self.i32_le()? }),
            4 => NbtValue::Long(if net { self.var_i64()? } else { self.i64_le()? }),
            5 => NbtValue::Float(self.f32_le()?),
            6 => NbtValue::Double(self.f64_le()?),
            7 => {
                let len = self.nbt_len(flavor)?;
                NbtValue::ByteArray(self.bytes(len, "NBT byte array")?.to_vec())
            }
            8 => NbtValue::String(self.read_nbt_string(flavor)?),
            9 => {
                let inner = self.u8()?;
                let len = self.nbt_len(flavor)?;
                let mut items = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    items.push(self.read_nbt_payload(inner, flavor, depth + 1)?);
                }
                NbtValue::List(inner, items)
            }
            10 => {
                let mut entries = Vec::new();
                loop {
                    let inner = self.u8()?;
                    if inner == 0 {
                        break;
                    }
                    let name = self.read_nbt_string(flavor)?;
                    let value = self.read_nbt_payload(inner, flavor, depth + 1)?;
                    entries.push((name, value));
                }
                NbtValue::Compound(entries)
            }
            11 => {
                let len = self.nbt_len(flavor)?;
                let mut items = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    items.push(if net { self.var_i32()? } else { self.i32_le()? });
                }
                NbtValue::IntArray(items)
            }
            12 => {
                let len = self.nbt_len(flavor)?;
                let mut items = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    items.push(if net { self.var_i64()? } else { self.i64_le()? });
                }
                NbtValue::LongArray(items)
            }
            _ => return Err(self.err("unknown NBT tag")),
        })
    }
}

const NBT_MAX_DEPTH: u32 = 64;

/// Bedrock NBT encoding flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NbtFlavor {
    /// Network NBT: varint lengths and zig-zag varint ints/longs.
    Network,
    /// Plain little-endian NBT (disk format, used for block-state hashing).
    LittleEndian,
}

/// Decoded NBT value.
#[derive(Debug, Clone, PartialEq)]
pub enum NbtValue {
    End,
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    /// List element tag id and values.
    List(u8, Vec<NbtValue>),
    /// Ordered compound entries.
    Compound(Vec<(String, NbtValue)>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl NbtValue {
    /// NBT tag id of this value.
    pub fn tag_id(&self) -> u8 {
        match self {
            NbtValue::End => 0,
            NbtValue::Byte(_) => 1,
            NbtValue::Short(_) => 2,
            NbtValue::Int(_) => 3,
            NbtValue::Long(_) => 4,
            NbtValue::Float(_) => 5,
            NbtValue::Double(_) => 6,
            NbtValue::ByteArray(_) => 7,
            NbtValue::String(_) => 8,
            NbtValue::List(..) => 9,
            NbtValue::Compound(_) => 10,
            NbtValue::IntArray(_) => 11,
            NbtValue::LongArray(_) => 12,
        }
    }

    /// Looks up a compound entry by name.
    pub fn get(&self, key: &str) -> Option<&NbtValue> {
        match self {
            NbtValue::Compound(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Returns the string payload, if any.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            NbtValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Writes this value as a named root tag in plain little-endian NBT.
    pub fn write_le_root(&self, name: &str, out: &mut Vec<u8>) {
        out.push(self.tag_id());
        put_le_nbt_string(name, out);
        self.write_le_payload(out);
    }

    fn write_le_payload(&self, out: &mut Vec<u8>) {
        match self {
            NbtValue::End => {}
            NbtValue::Byte(v) => out.push(*v as u8),
            NbtValue::Short(v) => out.extend_from_slice(&v.to_le_bytes()),
            NbtValue::Int(v) => out.extend_from_slice(&v.to_le_bytes()),
            NbtValue::Long(v) => out.extend_from_slice(&v.to_le_bytes()),
            NbtValue::Float(v) => out.extend_from_slice(&v.to_le_bytes()),
            NbtValue::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
            NbtValue::ByteArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_le_bytes());
                out.extend_from_slice(v);
            }
            NbtValue::String(s) => put_le_nbt_string(s, out),
            NbtValue::List(tag, items) => {
                out.push(*tag);
                out.extend_from_slice(&(items.len() as i32).to_le_bytes());
                for item in items {
                    item.write_le_payload(out);
                }
            }
            NbtValue::Compound(entries) => {
                for (k, v) in entries {
                    v.write_le_root(k, out);
                }
                out.push(0);
            }
            NbtValue::IntArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_le_bytes());
                for i in v {
                    out.extend_from_slice(&i.to_le_bytes());
                }
            }
            NbtValue::LongArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_le_bytes());
                for i in v {
                    out.extend_from_slice(&i.to_le_bytes());
                }
            }
        }
    }
}

fn put_le_nbt_string(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..len]);
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// Appends an unsigned LEB128 varint.
pub fn put_var_u32(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Appends a zig-zag signed varint.
pub fn put_var_i32(out: &mut Vec<u8>, value: i32) {
    put_var_u32(out, ((value << 1) ^ (value >> 31)) as u32);
}

/// Appends an unsigned 64-bit varint.
pub fn put_var_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Appends a zig-zag signed 64-bit varint.
pub fn put_var_i64(out: &mut Vec<u8>, value: i64) {
    put_var_u64(out, ((value << 1) ^ (value >> 63)) as u64);
}

/// Appends an unsigned 128-bit varint (PlayerAuthInput input flags).
pub fn put_var_u128(out: &mut Vec<u8>, mut value: u128) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Appends a little-endian `f32`.
pub fn put_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends three little-endian floats.
pub fn put_vec3(out: &mut Vec<u8>, v: [f32; 3]) {
    put_f32(out, v[0]);
    put_f32(out, v[1]);
    put_f32(out, v[2]);
}

/// Appends two little-endian floats.
pub fn put_vec2(out: &mut Vec<u8>, v: [f32; 2]) {
    put_f32(out, v[0]);
    put_f32(out, v[1]);
}

/// Appends a varuint32-prefixed string.
pub fn put_string(out: &mut Vec<u8>, s: &str) {
    put_var_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// Appends a block position as three zig-zag varints.
pub fn put_block_pos(out: &mut Vec<u8>, p: [i32; 3]) {
    put_var_i32(out, p[0]);
    put_var_i32(out, p[1]);
    put_var_i32(out, p[2]);
}

/// Appends a block position whose Y is an unsigned varint.
pub fn put_ublock_pos(out: &mut Vec<u8>, p: [i32; 3]) {
    put_var_i32(out, p[0]);
    put_var_u32(out, p[1] as u32);
    put_var_i32(out, p[2]);
}

// ---------------------------------------------------------------------------
// Batch framing
// ---------------------------------------------------------------------------

/// Starts a framed packet inside a batch. Returns a marker to pass to
/// [`end_packet`]. The body is written directly into `out`, avoiding a
/// temporary buffer per packet.
pub fn begin_packet(out: &mut Vec<u8>, packet_id: u32) -> usize {
    let mark = out.len();
    // Reserve 3 bytes for the length prefix (up to 2 MiB packets); the prefix
    // is shifted in `end_packet` if a different width is needed.
    out.extend_from_slice(&[0, 0, 0]);
    put_var_u32(out, packet_id & 0x3ff);
    mark
}

/// Finalises a packet started with [`begin_packet`].
pub fn end_packet(out: &mut Vec<u8>, mark: usize) {
    let body_len = out.len() - mark - 3;
    let mut prefix = [0u8; 5];
    let mut n = 0;
    let mut v = body_len as u32;
    loop {
        if v < 0x80 {
            prefix[n] = v as u8;
            n += 1;
            break;
        }
        prefix[n] = (v as u8 & 0x7f) | 0x80;
        n += 1;
        v >>= 7;
    }
    match n.cmp(&3) {
        std::cmp::Ordering::Equal => {}
        std::cmp::Ordering::Less => {
            out.drain(mark + n..mark + 3);
        }
        std::cmp::Ordering::Greater => {
            let extra = n - 3;
            for _ in 0..extra {
                out.insert(mark + 3, 0);
            }
        }
    }
    out[mark..mark + n].copy_from_slice(&prefix[..n]);
}

/// A single packet inside a decompressed batch.
#[derive(Debug, Clone, Copy)]
pub struct RawPacket<'a> {
    /// Packet id (lower 10 bits of the header).
    pub id: u32,
    /// Payload after the header.
    pub payload: &'a [u8],
}

/// Iterates over length-prefixed packets in a decompressed batch.
pub fn iter_packets(batch: &[u8]) -> PacketIter<'_> {
    PacketIter {
        reader: WireReader::new(batch),
        failed: false,
    }
}

/// Iterator returned by [`iter_packets`].
pub struct PacketIter<'a> {
    reader: WireReader<'a>,
    failed: bool,
}

impl<'a> Iterator for PacketIter<'a> {
    type Item = WireResult<RawPacket<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.reader.remaining() == 0 {
            return None;
        }
        let result = (|| {
            let packet = self.reader.byte_slice()?;
            let mut inner = WireReader::new(packet);
            let header = inner.var_u32()?;
            Ok(RawPacket {
                id: header & 0x3ff,
                payload: inner.rest(),
            })
        })();
        if result.is_err() {
            self.failed = true;
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        let mut out = Vec::new();
        for v in [0i32, 1, -1, 63, -64, 1 << 20, i32::MIN, i32::MAX] {
            out.clear();
            put_var_i32(&mut out, v);
            assert_eq!(WireReader::new(&out).var_i32().unwrap(), v);
        }
        for v in [0u64, 127, 128, u64::MAX] {
            out.clear();
            put_var_u64(&mut out, v);
            assert_eq!(WireReader::new(&out).var_u64().unwrap(), v);
        }
    }

    #[test]
    fn packet_framing_handles_all_prefix_widths() {
        for body in [1usize, 200, 20_000, 3_000_000] {
            let mut out = vec![0xAA];
            let mark = begin_packet(&mut out, 0x90);
            out.extend(std::iter::repeat_n(7u8, body));
            end_packet(&mut out, mark);
            let packets: Vec<_> = iter_packets(&out[1..]).collect::<Result<_, _>>().unwrap();
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].id, 0x90);
            assert_eq!(packets[0].payload.len(), body);
        }
    }

    #[test]
    fn network_nbt_parse_and_skip() {
        // compound "" { string "name": "minecraft:stone", int "v": -3 }
        let mut buf = vec![10, 0];
        buf.push(8);
        put_var_u32(&mut buf, 4);
        buf.extend_from_slice(b"name");
        put_string(&mut buf, "minecraft:stone");
        buf.push(3);
        put_var_u32(&mut buf, 1);
        buf.extend_from_slice(b"v");
        put_var_i32(&mut buf, -3);
        buf.push(0);
        buf.push(0xEE);

        let mut r = WireReader::new(&buf);
        let (_, v) = r.read_nbt(NbtFlavor::Network).unwrap();
        assert_eq!(
            v.get("name").and_then(NbtValue::as_str),
            Some("minecraft:stone")
        );
        assert_eq!(v.get("v"), Some(&NbtValue::Int(-3)));
        assert_eq!(r.u8().unwrap(), 0xEE);

        let mut r = WireReader::new(&buf);
        r.skip_nbt(NbtFlavor::Network).unwrap();
        assert_eq!(r.u8().unwrap(), 0xEE);
    }
}
