pub mod wire;

use bytes::{Buf, BufMut};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("unexpected end of buffer: {0}")]
    UnexpectedEof(&'static str),
    #[error("invalid UTF-8 string: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("invalid VarInt value")]
    InvalidVarInt,
    #[error("invalid NBT structure: {0}")]
    InvalidNbt(String),
}

// Vector types
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Vector3f {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Vector2f {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BlockPosition {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

// Item / Entity Metadata Base Types
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityMetadataEntry {
    pub id: u32,
    pub kind: u32,
    pub value: MetadataValue,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MetadataValue {
    Byte(u8),
    Short(i16),
    Int(i32),
    Float(f32),
    String(String),
    Compound(Vec<u8>),
    Position(BlockPosition),
    Long(i64),
    Vector(Vector3f),
}

// VarInt readers/writers operating on Buf/BufMut or raw slices
pub fn put_var_u32<B: BufMut>(buf: &mut B, mut value: u32) {
    loop {
        if (value & !0x7f) == 0 {
            buf.put_u8(value as u8);
            break;
        }
        buf.put_u8(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
}

pub fn get_var_u32<B: Buf>(buf: &mut B) -> Result<u32, CoreError> {
    let mut value = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        if buf.remaining() < 1 {
            return Err(CoreError::UnexpectedEof("var_u32"));
        }
        let byte = buf.get_u8();
        value |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(CoreError::InvalidVarInt)
}

pub fn put_var_i32<B: BufMut>(buf: &mut B, value: i32) {
    put_var_u32(buf, value as u32);
}

pub fn get_var_i32<B: Buf>(buf: &mut B) -> Result<i32, CoreError> {
    get_var_u32(buf).map(|v| v as i32)
}

pub fn put_zigzag_i32<B: BufMut>(buf: &mut B, value: i32) {
    let uval = ((value << 1) ^ (value >> 31)) as u32;
    put_var_u32(buf, uval);
}

pub fn get_zigzag_i32<B: Buf>(buf: &mut B) -> Result<i32, CoreError> {
    let uval = get_var_u32(buf)?;
    Ok(((uval >> 1) as i32) ^ (-((uval & 1) as i32)))
}

pub fn put_var_u64<B: BufMut>(buf: &mut B, mut value: u64) {
    loop {
        if (value & !0x7f) == 0 {
            buf.put_u8(value as u8);
            break;
        }
        buf.put_u8(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
}

pub fn get_var_u64<B: Buf>(buf: &mut B) -> Result<u64, CoreError> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for _ in 0..10 {
        if buf.remaining() < 1 {
            return Err(CoreError::UnexpectedEof("var_u64"));
        }
        let byte = buf.get_u8();
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(CoreError::InvalidVarInt)
}

pub fn put_zigzag_i64<B: BufMut>(buf: &mut B, value: i64) {
    let uval = ((value << 1) ^ (value >> 63)) as u64;
    put_var_u64(buf, uval);
}

pub fn get_zigzag_i64<B: Buf>(buf: &mut B) -> Result<i64, CoreError> {
    let uval = get_var_u64(buf)?;
    Ok(((uval >> 1) as i64) ^ (-((uval & 1) as i64)))
}

// String helpers (MCPE-style, length prefix is var_u32)
pub fn put_string<B: BufMut>(buf: &mut B, val: &str) {
    put_var_u32(buf, val.len() as u32);
    buf.put_slice(val.as_bytes());
}

pub fn get_string<B: Buf>(buf: &mut B) -> Result<String, CoreError> {
    let len = get_var_u32(buf)? as usize;
    if buf.remaining() < len {
        return Err(CoreError::UnexpectedEof("string"));
    }
    let mut bytes = vec![0u8; len];
    buf.copy_to_slice(&mut bytes);
    String::from_utf8(bytes).map_err(Into::into)
}

// LE String helper (for custom authentication structures or legacy packets)
pub fn put_le_string<B: BufMut>(buf: &mut B, val: &str) {
    buf.put_u32_le(val.len() as u32);
    buf.put_slice(val.as_bytes());
}

pub fn get_le_string<B: Buf>(buf: &mut B) -> Result<String, CoreError> {
    if buf.remaining() < 4 {
        return Err(CoreError::UnexpectedEof("le_string length"));
    }
    let len = buf.get_u32_le() as usize;
    if buf.remaining() < len {
        return Err(CoreError::UnexpectedEof("le_string"));
    }
    let mut bytes = vec![0u8; len];
    buf.copy_to_slice(&mut bytes);
    String::from_utf8(bytes).map_err(Into::into)
}

// UUID helper
pub fn put_uuid<B: BufMut>(buf: &mut B, uuid: &Uuid) {
    buf.put_slice(uuid.as_bytes());
}

pub fn get_uuid<B: Buf>(buf: &mut B) -> Result<Uuid, CoreError> {
    if buf.remaining() < 16 {
        return Err(CoreError::UnexpectedEof("uuid"));
    }
    let mut bytes = [0u8; 16];
    buf.copy_to_slice(&mut bytes);
    Ok(Uuid::from_bytes(bytes))
}

// NBT helper (skips or decodes NBT compound payload)
pub fn skip_nbt<B: Buf>(buf: &mut B) -> Result<(), CoreError> {
    if buf.remaining() < 1 {
        return Err(CoreError::UnexpectedEof("NBT tag"));
    }
    let tag = buf.get_u8();
    if tag == 0 {
        return Ok(());
    }
    skip_nbt_string(buf)?;
    skip_nbt_payload(buf, tag)
}

fn skip_nbt_string<B: Buf>(buf: &mut B) -> Result<(), CoreError> {
    if buf.remaining() < 2 {
        return Err(CoreError::UnexpectedEof("NBT string length"));
    }
    let len = buf.get_u16_le() as usize;
    if buf.remaining() < len {
        return Err(CoreError::UnexpectedEof("NBT string content"));
    }
    buf.advance(len);
    Ok(())
}

fn skip_nbt_payload<B: Buf>(buf: &mut B, tag: u8) -> Result<(), CoreError> {
    match tag {
        0 => Ok(()),
        1 => {
            // Byte
            if buf.remaining() < 1 {
                return Err(CoreError::UnexpectedEof("NBT byte"));
            }
            buf.get_u8();
            Ok(())
        }
        2 => {
            // Short
            if buf.remaining() < 2 {
                return Err(CoreError::UnexpectedEof("NBT short"));
            }
            buf.get_i16_le();
            Ok(())
        }
        3 => {
            // Int
            if buf.remaining() < 4 {
                return Err(CoreError::UnexpectedEof("NBT int"));
            }
            buf.get_i32_le();
            Ok(())
        }
        4 => {
            // Long
            if buf.remaining() < 8 {
                return Err(CoreError::UnexpectedEof("NBT long"));
            }
            buf.get_i64_le();
            Ok(())
        }
        5 => {
            // Float
            if buf.remaining() < 4 {
                return Err(CoreError::UnexpectedEof("NBT float"));
            }
            buf.get_f32_le();
            Ok(())
        }
        6 => {
            // Double
            if buf.remaining() < 8 {
                return Err(CoreError::UnexpectedEof("NBT double"));
            }
            buf.get_f64_le();
            Ok(())
        }
        7 => {
            // Byte Array
            if buf.remaining() < 4 {
                return Err(CoreError::UnexpectedEof("NBT byte array len"));
            }
            let len = buf.get_i32_le() as usize;
            if buf.remaining() < len {
                return Err(CoreError::UnexpectedEof("NBT byte array body"));
            }
            buf.advance(len);
            Ok(())
        }
        8 => {
            // String
            skip_nbt_string(buf)
        }
        9 => {
            // List
            if buf.remaining() < 5 {
                return Err(CoreError::UnexpectedEof("NBT list header"));
            }
            let sub_tag = buf.get_u8();
            let len = buf.get_i32_le() as usize;
            for _ in 0..len {
                skip_nbt_payload(buf, sub_tag)?;
            }
            Ok(())
        }
        10 => {
            // Compound
            loop {
                if buf.remaining() < 1 {
                    return Err(CoreError::UnexpectedEof("NBT compound tag"));
                }
                let sub_tag = buf.get_u8();
                if sub_tag == 0 {
                    break;
                }
                skip_nbt_string(buf)?;
                skip_nbt_payload(buf, sub_tag)?;
            }
            Ok(())
        }
        11 => {
            // Int Array
            if buf.remaining() < 4 {
                return Err(CoreError::UnexpectedEof("NBT int array len"));
            }
            let len = buf.get_i32_le() as usize;
            if buf.remaining() < len * 4 {
                return Err(CoreError::UnexpectedEof("NBT int array body"));
            }
            buf.advance(len * 4);
            Ok(())
        }
        12 => {
            // Long Array
            if buf.remaining() < 4 {
                return Err(CoreError::UnexpectedEof("NBT long array len"));
            }
            let len = buf.get_i32_le() as usize;
            if buf.remaining() < len * 8 {
                return Err(CoreError::UnexpectedEof("NBT long array body"));
            }
            buf.advance(len * 8);
            Ok(())
        }
        other => Err(CoreError::InvalidNbt(format!("unknown NBT tag: {other}"))),
    }
}
