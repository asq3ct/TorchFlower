//! Zero-copy decoding of the game packets the bot consumes, and encoders for
//! the packets it sends. Layouts follow protocol 898 (1.21.130) and are
//! unchanged through 1.26.30 unless noted.

use torchflower_inventory::ItemStack;
use torchflower_protocol_core::wire::{
    begin_packet, end_packet, put_f32, put_string, put_ublock_pos, put_var_i32, put_var_u128,
    put_var_u32, put_var_u64, put_vec2, put_vec3, NbtFlavor, WireError, WireReader,
};

/// Packet ids.
pub mod id {
    pub const PLAY_STATUS: u32 = 0x02;
    pub const DISCONNECT: u32 = 0x05;
    pub const RESOURCE_PACKS_INFO: u32 = 0x06;
    pub const RESOURCE_PACK_STACK: u32 = 0x07;
    pub const TEXT: u32 = 0x09;
    pub const SET_TIME: u32 = 0x0a;
    pub const START_GAME: u32 = 0x0b;
    pub const ADD_PLAYER: u32 = 0x0c;
    pub const ADD_ACTOR: u32 = 0x0d;
    pub const REMOVE_ACTOR: u32 = 0x0e;
    pub const ADD_ITEM_ACTOR: u32 = 0x0f;
    pub const TAKE_ITEM_ACTOR: u32 = 0x11;
    pub const MOVE_ACTOR_ABSOLUTE: u32 = 0x12;
    pub const MOVE_PLAYER: u32 = 0x13;
    pub const UPDATE_BLOCK: u32 = 0x15;
    pub const UPDATE_ATTRIBUTES: u32 = 0x1d;
    pub const INVENTORY_TRANSACTION: u32 = 0x1e;
    pub const MOB_EQUIPMENT: u32 = 0x1f;
    pub const INTERACT: u32 = 0x21;
    pub const PLAYER_ACTION: u32 = 0x24;
    pub const SET_ACTOR_MOTION: u32 = 0x28;
    pub const SET_HEALTH: u32 = 0x2a;
    pub const ANIMATE: u32 = 0x2c;
    pub const RESPAWN: u32 = 0x2d;
    pub const CONTAINER_OPEN: u32 = 0x2e;
    pub const CONTAINER_CLOSE: u32 = 0x2f;
    pub const PLAYER_HOTBAR: u32 = 0x30;
    pub const INVENTORY_CONTENT: u32 = 0x31;
    pub const INVENTORY_SLOT: u32 = 0x32;
    pub const CRAFTING_DATA: u32 = 0x34;
    pub const LEVEL_CHUNK: u32 = 0x3a;
    pub const CHANGE_DIMENSION: u32 = 0x3d;
    pub const CHUNK_RADIUS_UPDATED: u32 = 0x46;
    pub const COMMAND_REQUEST: u32 = 0x4d;
    pub const MOVE_ACTOR_DELTA: u32 = 0x6f;
    pub const NETWORK_STACK_LATENCY: u32 = 0x73;
    pub const NETWORK_CHUNK_PUBLISHER_UPDATE: u32 = 0x79;
    pub const PLAYER_AUTH_INPUT: u32 = 0x90;
    pub const ITEM_STACK_RESPONSE: u32 = 0x94;
    pub const CORRECT_PLAYER_MOVE_PREDICTION: u32 = 0xa1;
    pub const ITEM_REGISTRY: u32 = 0xa2;
    pub const SUB_CHUNK: u32 = 0xae;
}

/// First protocol using the categorised `Text` layout (1.21.130).
pub const TEXT_CATEGORY_PROTOCOL: i32 = 898;

/// PlayStatus values.
pub mod play_status {
    pub const LOGIN_SUCCESS: i32 = 0;
    pub const PLAYER_SPAWN: i32 = 3;
}

/// Decoded chat/system message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMessage {
    pub kind: u8,
    pub source: String,
    pub message: String,
    pub parameters: Vec<String>,
    pub xuid: String,
}

/// Text types.
pub mod text_type {
    pub const RAW: u8 = 0;
    pub const CHAT: u8 = 1;
    pub const TRANSLATION: u8 = 2;
    pub const POPUP: u8 = 3;
    pub const JUKEBOX_POPUP: u8 = 4;
    pub const TIP: u8 = 5;
    pub const SYSTEM: u8 = 6;
    pub const WHISPER: u8 = 7;
    pub const ANNOUNCEMENT: u8 = 8;
    pub const OBJECT_WHISPER: u8 = 9;
    pub const OBJECT: u8 = 10;
    pub const OBJECT_ANNOUNCEMENT: u8 = 11;
}

fn text_body(r: &mut WireReader<'_>, kind: u8) -> Result<(String, String, Vec<String>), WireError> {
    use text_type::*;
    let mut source = String::new();
    let mut params = Vec::new();
    let message = match kind {
        CHAT | WHISPER | ANNOUNCEMENT => {
            source = r.string()?.to_string();
            r.string()?.to_string()
        }
        TRANSLATION | POPUP | JUKEBOX_POPUP => {
            let m = r.string()?.to_string();
            let n = r.var_u32()?.min(64);
            for _ in 0..n {
                params.push(r.string()?.to_string());
            }
            m
        }
        _ => r.string()?.to_string(),
    };
    Ok((source, message, params))
}

impl TextMessage {
    /// Decodes a `Text` payload for the given protocol.
    pub fn decode(payload: &[u8], protocol: i32) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        let kind;
        if protocol >= TEXT_CATEGORY_PROTOCOL {
            r.bool()?;
            let category = r.u8()?;
            let consts = match category {
                0 => 6,
                _ => 3,
            };
            for _ in 0..consts {
                r.string()?;
            }
            kind = r.u8()?;
        } else {
            kind = r.u8()?;
            r.bool()?;
        }
        let (source, message, parameters) = text_body(&mut r, kind)?;
        let xuid = r.string().unwrap_or_default().to_string();
        Ok(Self {
            kind,
            source,
            message,
            parameters,
            xuid,
        })
    }
}

/// Appends a chat `Text` packet.
pub fn encode_chat(out: &mut Vec<u8>, protocol: i32, source: &str, message: &str, xuid: &str) {
    let mark = begin_packet(out, id::TEXT);
    if protocol >= TEXT_CATEGORY_PROTOCOL {
        out.push(0); // needs translation
        out.push(1); // authored message
        for c in ["chat", "whisper", "announcement"] {
            put_string(out, c);
        }
        out.push(text_type::CHAT);
    } else {
        out.push(text_type::CHAT);
        out.push(0);
    }
    put_string(out, source);
    put_string(out, message);
    put_string(out, xuid);
    put_string(out, ""); // platform chat id
    if protocol >= TEXT_CATEGORY_PROTOCOL {
        out.push(0); // no filtered message
    } else {
        put_string(out, "");
    }
    end_packet(out, mark);
}

/// Appends a `CommandRequest` for `command` (leading `/` optional).
pub fn encode_command(out: &mut Vec<u8>, command: &str, uuid: [u8; 16], request_id: &str) {
    let mark = begin_packet(out, id::COMMAND_REQUEST);
    if command.starts_with('/') {
        put_string(out, command);
    } else {
        put_string(out, &format!("/{command}"));
    }
    put_string(out, "player");
    out.extend_from_slice(&uuid);
    put_string(out, request_id);
    out.extend_from_slice(&0i64.to_le_bytes());
    out.push(0);
    put_string(out, "52");
    end_packet(out, mark);
}

/// Prefix of `StartGame` needed by the bot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StartGameInfo {
    pub unique_id: i64,
    pub runtime_id: u64,
    pub game_mode: i32,
    /// Eye position.
    pub position: [f32; 3],
    pub pitch: f32,
    pub yaw: f32,
    pub dimension: i32,
}

impl StartGameInfo {
    pub fn decode(payload: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        let unique_id = r.var_i64()?;
        let runtime_id = r.var_u64()?;
        let game_mode = r.var_i32()?;
        let position = r.vec3()?;
        let pitch = r.f32_le()?;
        let yaw = r.f32_le()?;
        r.skip(8, "seed")?;
        r.skip(2, "biome type")?;
        r.string()?;
        let dimension = r.var_i32()?;
        Ok(Self {
            unique_id,
            runtime_id,
            game_mode,
            position,
            pitch,
            yaw,
            dimension,
        })
    }
}

/// Skips an entity metadata map.
pub fn skip_entity_metadata(r: &mut WireReader<'_>) -> Result<(), WireError> {
    let n = r.var_u32()?;
    for _ in 0..n {
        r.var_u32()?;
        match r.var_u32()? {
            0 => r.skip(1, "meta byte")?,
            1 => r.skip(2, "meta short")?,
            2 => {
                r.var_i32()?;
            }
            3 => r.skip(4, "meta float")?,
            4 => {
                r.string()?;
            }
            5 => r.skip_nbt(NbtFlavor::Network)?,
            6 => {
                r.block_pos()?;
            }
            7 => {
                r.var_i64()?;
            }
            8 => r.skip(12, "meta vec3")?,
            _ => return Err(r.err("entity metadata type")),
        }
    }
    Ok(())
}

fn skip_entity_properties(r: &mut WireReader<'_>) -> Result<(), WireError> {
    let ints = r.var_u32()?;
    for _ in 0..ints {
        r.var_u32()?;
        r.var_i32()?;
    }
    let floats = r.var_u32()?;
    for _ in 0..floats {
        r.var_u32()?;
        r.skip(4, "float property")?;
    }
    Ok(())
}

/// Spawned entity (players, mobs, item drops).
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnInfo<'a> {
    pub unique_id: Option<i64>,
    pub runtime_id: u64,
    pub kind: &'a str,
    pub username: Option<&'a str>,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub pitch: f32,
    pub yaw: f32,
    pub item: Option<ItemStack>,
}

/// Decoded inbound packet.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound<'a> {
    PlayStatus(i32),
    Disconnect(String),
    ResourcePacksInfo,
    ResourcePackStack,
    Text(TextMessage),
    SetTime(i32),
    StartGame(StartGameInfo, &'a [u8]),
    Spawn(SpawnInfo<'a>),
    RemoveActor(i64),
    TakeItemActor {
        item: u64,
        taker: u64,
    },
    MoveActor {
        runtime_id: u64,
        position: Option<[Option<f32>; 3]>,
        yaw: Option<f32>,
        pitch: Option<f32>,
        on_ground: bool,
        teleport: bool,
    },
    MovePlayer {
        runtime_id: u64,
        position: [f32; 3],
        pitch: f32,
        yaw: f32,
        mode: u8,
        on_ground: bool,
    },
    CorrectMove {
        position: [f32; 3],
        delta: [f32; 3],
        on_ground: bool,
    },
    SetActorMotion {
        runtime_id: u64,
        velocity: [f32; 3],
    },
    SetHealth(i32),
    Attributes {
        runtime_id: u64,
        values: Vec<(&'a str, f32, f32)>,
    },
    Respawn {
        position: [f32; 3],
        state: u8,
        runtime_id: u64,
    },
    UpdateBlock {
        pos: [i32; 3],
        runtime_id: u32,
        layer: u32,
    },
    LevelChunk(&'a [u8]),
    SubChunk(&'a [u8]),
    InventoryContent(&'a [u8]),
    InventorySlot(&'a [u8]),
    ContainerOpen(&'a [u8]),
    ContainerClose(&'a [u8]),
    ItemStackResponse(&'a [u8]),
    ItemRegistry(&'a [u8]),
    CraftingData(&'a [u8]),
    NetworkStackLatency {
        timestamp: i64,
        needs_response: bool,
    },
    ChangeDimension {
        dimension: i32,
        position: [f32; 3],
    },
    MobEquipment {
        runtime_id: u64,
        item: ItemStack,
        hotbar_slot: u8,
        window: u8,
    },
    PlayerHotbar {
        slot: u32,
        window: u8,
        select: bool,
    },
    ChunkRadiusUpdated(i32),
    Other(u32),
}

/// MoveActorDelta flags.
const DELTA_X: u16 = 1;
const DELTA_Y: u16 = 2;
const DELTA_Z: u16 = 4;
const DELTA_ROT_X: u16 = 8;
const DELTA_ROT_Y: u16 = 16;
const DELTA_ROT_Z: u16 = 32;
const DELTA_ON_GROUND: u16 = 64;
const DELTA_TELEPORT: u16 = 128;

impl<'a> Inbound<'a> {
    /// Decodes a packet payload.
    pub fn decode(packet_id: u32, p: &'a [u8], protocol: i32) -> Result<Self, WireError> {
        let mut r = WireReader::new(p);
        Ok(match packet_id {
            id::PLAY_STATUS => {
                let b = r.bytes(4, "play status")?;
                Inbound::PlayStatus(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            }
            id::DISCONNECT => {
                r.var_i32()?;
                let hide = r.bool()?;
                let msg = if hide { "" } else { r.string().unwrap_or("") };
                Inbound::Disconnect(msg.to_string())
            }
            id::RESOURCE_PACKS_INFO => Inbound::ResourcePacksInfo,
            id::RESOURCE_PACK_STACK => Inbound::ResourcePackStack,
            id::TEXT => Inbound::Text(TextMessage::decode(p, protocol)?),
            id::SET_TIME => Inbound::SetTime(r.var_i32()?),
            id::START_GAME => Inbound::StartGame(StartGameInfo::decode(p)?, p),
            id::ADD_PLAYER => {
                r.skip(16, "uuid")?;
                let username = r.string()?;
                let runtime_id = r.var_u64()?;
                r.string()?;
                let position = r.vec3()?;
                let velocity = r.vec3()?;
                let pitch = r.f32_le()?;
                let yaw = r.f32_le()?;
                r.f32_le()?;
                let item = ItemStack::read_instance(&mut r)?;
                // Unique id lives in the ability data after metadata/properties.
                let unique_id = (|| -> Result<i64, WireError> {
                    r.var_i32()?;
                    skip_entity_metadata(&mut r)?;
                    skip_entity_properties(&mut r)?;
                    r.i64_le()
                })()
                .ok();
                Inbound::Spawn(SpawnInfo {
                    unique_id,
                    runtime_id,
                    kind: "minecraft:player",
                    username: Some(username),
                    position,
                    velocity,
                    pitch,
                    yaw,
                    item: (!item.is_empty()).then_some(item),
                })
            }
            id::ADD_ACTOR => {
                let unique_id = r.var_i64()?;
                let runtime_id = r.var_u64()?;
                let kind = r.string()?;
                let position = r.vec3()?;
                let velocity = r.vec3()?;
                let pitch = r.f32_le()?;
                let yaw = r.f32_le()?;
                Inbound::Spawn(SpawnInfo {
                    unique_id: Some(unique_id),
                    runtime_id,
                    kind,
                    username: None,
                    position,
                    velocity,
                    pitch,
                    yaw,
                    item: None,
                })
            }
            id::ADD_ITEM_ACTOR => {
                let unique_id = r.var_i64()?;
                let runtime_id = r.var_u64()?;
                let item = ItemStack::read_instance(&mut r)?;
                let position = r.vec3()?;
                let velocity = r.vec3()?;
                Inbound::Spawn(SpawnInfo {
                    unique_id: Some(unique_id),
                    runtime_id,
                    kind: "minecraft:item",
                    username: None,
                    position,
                    velocity,
                    pitch: 0.0,
                    yaw: 0.0,
                    item: Some(item),
                })
            }
            id::REMOVE_ACTOR => Inbound::RemoveActor(r.var_i64()?),
            id::TAKE_ITEM_ACTOR => Inbound::TakeItemActor {
                item: r.var_u64()?,
                taker: r.var_u64()?,
            },
            id::MOVE_ACTOR_ABSOLUTE => {
                let runtime_id = r.var_u64()?;
                let flags = r.u8()?;
                let pos = r.vec3()?;
                let pitch = r.byte_angle()?;
                let yaw = r.byte_angle()?;
                let _head_yaw = r.byte_angle()?;
                Inbound::MoveActor {
                    runtime_id,
                    position: Some([Some(pos[0]), Some(pos[1]), Some(pos[2])]),
                    yaw: Some(yaw),
                    pitch: Some(pitch),
                    on_ground: flags & 1 != 0,
                    teleport: flags & 2 != 0,
                }
            }
            id::MOVE_ACTOR_DELTA => {
                let runtime_id = r.var_u64()?;
                let flags = r.u16_le()?;
                let mut pos = [None; 3];
                for (i, bit) in [DELTA_X, DELTA_Y, DELTA_Z].into_iter().enumerate() {
                    if flags & bit != 0 {
                        pos[i] = Some(r.f32_le()?);
                    }
                }
                let pitch = if flags & DELTA_ROT_X != 0 {
                    Some(r.byte_angle()?)
                } else {
                    None
                };
                let yaw = if flags & DELTA_ROT_Y != 0 {
                    Some(r.byte_angle()?)
                } else {
                    None
                };
                if flags & DELTA_ROT_Z != 0 {
                    r.byte_angle()?;
                }
                Inbound::MoveActor {
                    runtime_id,
                    position: Some(pos),
                    yaw,
                    pitch,
                    on_ground: flags & DELTA_ON_GROUND != 0,
                    teleport: flags & DELTA_TELEPORT != 0,
                }
            }
            id::MOVE_PLAYER => {
                let runtime_id = r.var_u64()?;
                let position = r.vec3()?;
                let pitch = r.f32_le()?;
                let yaw = r.f32_le()?;
                r.f32_le()?;
                let mode = r.u8()?;
                let on_ground = r.bool()?;
                Inbound::MovePlayer {
                    runtime_id,
                    position,
                    pitch,
                    yaw,
                    mode,
                    on_ground,
                }
            }
            id::CORRECT_PLAYER_MOVE_PREDICTION => {
                r.u8()?;
                let position = r.vec3()?;
                let delta = r.vec3()?;
                r.vec2()?;
                if r.bool()? {
                    r.f32_le()?;
                }
                let on_ground = r.bool()?;
                Inbound::CorrectMove {
                    position,
                    delta,
                    on_ground,
                }
            }
            id::SET_ACTOR_MOTION => Inbound::SetActorMotion {
                runtime_id: r.var_u64()?,
                velocity: r.vec3()?,
            },
            id::SET_HEALTH => Inbound::SetHealth(r.var_i32()?),
            id::UPDATE_ATTRIBUTES => {
                let runtime_id = r.var_u64()?;
                let n = r.var_u32()?.min(64);
                let mut values = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let _min = r.f32_le()?;
                    let max = r.f32_le()?;
                    let value = r.f32_le()?;
                    r.skip(12, "attribute defaults")?;
                    let name = r.string()?;
                    let mods = r.var_u32()?;
                    for _ in 0..mods {
                        r.string()?;
                        r.string()?;
                        r.skip(4 + 4 + 4 + 1, "attribute modifier")?;
                    }
                    values.push((name, value, max));
                }
                Inbound::Attributes { runtime_id, values }
            }
            id::RESPAWN => Inbound::Respawn {
                position: r.vec3()?,
                state: r.u8()?,
                runtime_id: r.var_u64()?,
            },
            id::UPDATE_BLOCK => Inbound::UpdateBlock {
                pos: r.ublock_pos()?,
                runtime_id: r.var_u32()?,
                layer: {
                    r.var_u32()?;
                    r.var_u32()?
                },
            },
            id::LEVEL_CHUNK => Inbound::LevelChunk(p),
            id::SUB_CHUNK => Inbound::SubChunk(p),
            id::INVENTORY_CONTENT => Inbound::InventoryContent(p),
            id::INVENTORY_SLOT => Inbound::InventorySlot(p),
            id::CONTAINER_OPEN => Inbound::ContainerOpen(p),
            id::CONTAINER_CLOSE => Inbound::ContainerClose(p),
            id::ITEM_STACK_RESPONSE => Inbound::ItemStackResponse(p),
            id::ITEM_REGISTRY => Inbound::ItemRegistry(p),
            id::CRAFTING_DATA => Inbound::CraftingData(p),
            id::NETWORK_STACK_LATENCY => Inbound::NetworkStackLatency {
                timestamp: r.i64_le()?,
                needs_response: r.bool()?,
            },
            id::CHANGE_DIMENSION => Inbound::ChangeDimension {
                dimension: r.var_i32()?,
                position: r.vec3()?,
            },
            id::MOB_EQUIPMENT => Inbound::MobEquipment {
                runtime_id: r.var_u64()?,
                item: ItemStack::read_instance(&mut r)?,
                hotbar_slot: {
                    r.u8()?;
                    r.u8()?
                },
                window: r.u8()?,
            },
            id::PLAYER_HOTBAR => Inbound::PlayerHotbar {
                slot: r.var_u32()?,
                window: r.u8()?,
                select: r.bool()?,
            },
            id::CHUNK_RADIUS_UPDATED => Inbound::ChunkRadiusUpdated(r.var_i32()?),
            other => Inbound::Other(other),
        })
    }
}

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

/// Block action inside `PlayerAuthInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockAction {
    pub action: i32,
    pub pos: [i32; 3],
    pub face: i32,
}

/// Block action ids.
pub mod block_action {
    pub const START_BREAK: i32 = 0;
    pub const ABORT_BREAK: i32 = 1;
    pub const STOP_BREAK: i32 = 2;
    pub const RESPAWN: i32 = 7;
    pub const DIMENSION_CHANGE_DONE: i32 = 14;
    pub const CRACK_BREAK: i32 = 18;
    pub const PREDICT_DESTROY_BLOCK: i32 = 26;
    pub const CONTINUE_DESTROY_BLOCK: i32 = 27;
}

/// `UseItem` transaction data.
#[derive(Debug, Clone, PartialEq)]
pub struct UseItem<'a> {
    /// 0 click block, 1 click air, 2 break block.
    pub action: u32,
    pub block_pos: [i32; 3],
    pub face: i32,
    pub hotbar_slot: i32,
    pub held: &'a ItemStack,
    /// Player (eye) position.
    pub player_pos: [f32; 3],
    /// Click position relative to the block (0..1).
    pub click_pos: [f32; 3],
    pub block_runtime_id: u32,
}

fn put_use_item(out: &mut Vec<u8>, u: &UseItem<'_>) {
    put_var_u32(out, u.action);
    put_var_u32(out, 1); // trigger: player input
    put_ublock_pos(out, u.block_pos);
    put_var_i32(out, u.face);
    put_var_i32(out, u.hotbar_slot);
    u.held.write_instance(out);
    put_vec3(out, u.player_pos);
    put_vec3(out, u.click_pos);
    put_var_u32(out, u.block_runtime_id);
    put_var_u32(out, 1); // client prediction: success
}

/// Appends a standalone `InventoryTransaction` with use-item data.
pub fn encode_use_item(out: &mut Vec<u8>, u: &UseItem<'_>) {
    let mark = begin_packet(out, id::INVENTORY_TRANSACTION);
    put_var_i32(out, 0); // legacy request id
    put_var_u32(out, 2); // transaction type: use item
    put_var_u32(out, 0); // actions
    put_use_item(out, u);
    end_packet(out, mark);
}

/// Appends an `InventoryTransaction` attacking (action 1) or interacting
/// (action 0) with an entity.
pub fn encode_use_item_on_entity(
    out: &mut Vec<u8>,
    target: u64,
    action: u32,
    hotbar_slot: i32,
    held: &ItemStack,
    player_pos: [f32; 3],
) {
    let mark = begin_packet(out, id::INVENTORY_TRANSACTION);
    put_var_i32(out, 0);
    put_var_u32(out, 3);
    put_var_u32(out, 0);
    put_var_u64(out, target);
    put_var_u32(out, action);
    put_var_i32(out, hotbar_slot);
    held.write_instance(out);
    put_vec3(out, player_pos);
    put_vec3(out, [0.0; 3]);
    end_packet(out, mark);
}

/// Per-tick `PlayerAuthInput` fields.
#[derive(Debug, Clone)]
pub struct AuthInput<'a> {
    pub pitch: f32,
    pub yaw: f32,
    /// Eye position.
    pub position: [f32; 3],
    pub move_vector: [f32; 2],
    pub head_yaw: f32,
    pub flags: u128,
    pub tick: u64,
    pub delta: [f32; 3],
    pub camera: [f32; 3],
    pub raw_move_vector: [f32; 2],
    pub block_actions: &'a [BlockAction],
    pub item_stack_request: Option<&'a torchflower_inventory::StackRequest>,
}

/// Appends a `PlayerAuthInput` packet (protocol 766–1001 layout).
pub fn encode_auth_input(out: &mut Vec<u8>, a: &AuthInput<'_>) {
    use torchflower_physics::input_flags as f;
    let mut flags = a.flags & !(f::PERFORM_ITEM_INTERACTION | f::CLIENT_PREDICTED_VEHICLE);
    if a.block_actions.is_empty() {
        flags &= !f::PERFORM_BLOCK_ACTIONS;
    } else {
        flags |= f::PERFORM_BLOCK_ACTIONS;
    }
    if a.item_stack_request.is_some() {
        flags |= f::PERFORM_ITEM_STACK_REQUEST;
    } else {
        flags &= !f::PERFORM_ITEM_STACK_REQUEST;
    }
    let mark = begin_packet(out, id::PLAYER_AUTH_INPUT);
    put_f32(out, a.pitch);
    put_f32(out, a.yaw);
    put_vec3(out, a.position);
    put_vec2(out, a.move_vector);
    put_f32(out, a.head_yaw);
    put_var_u128(out, flags);
    put_var_u32(out, 1); // input mode: mouse
    put_var_u32(out, 0); // play mode: normal
    put_var_u32(out, 1); // interaction model: crosshair
    put_f32(out, a.pitch);
    put_f32(out, a.yaw);
    put_var_u64(out, a.tick);
    put_vec3(out, a.delta);
    if let Some(req) = a.item_stack_request {
        req.encode(out);
    }
    if !a.block_actions.is_empty() {
        put_var_i32(out, a.block_actions.len() as i32);
        for b in a.block_actions {
            put_var_i32(out, b.action);
            if matches!(b.action, 0 | 1 | 18 | 26 | 27) {
                torchflower_protocol_core::wire::put_block_pos(out, b.pos);
                put_var_i32(out, b.face);
            }
        }
    }
    put_vec2(out, a.move_vector);
    put_vec3(out, a.camera);
    put_vec2(out, a.raw_move_vector);
    end_packet(out, mark);
}

/// Appends `MobEquipment` selecting `hotbar_slot`.
pub fn encode_mob_equipment(out: &mut Vec<u8>, runtime_id: u64, item: &ItemStack, hotbar_slot: u8) {
    let mark = begin_packet(out, id::MOB_EQUIPMENT);
    put_var_u64(out, runtime_id);
    item.write_instance(out);
    out.push(hotbar_slot);
    out.push(hotbar_slot);
    out.push(0);
    end_packet(out, mark);
}

/// Appends an arm-swing `Animate`.
pub fn encode_swing(out: &mut Vec<u8>, runtime_id: u64) {
    let mark = begin_packet(out, id::ANIMATE);
    out.push(1);
    put_var_u64(out, runtime_id);
    put_f32(out, 0.0);
    out.push(0); // no swing source
    end_packet(out, mark);
}

/// Appends a legacy `PlayerAction`.
pub fn encode_player_action(
    out: &mut Vec<u8>,
    runtime_id: u64,
    action: i32,
    pos: [i32; 3],
    face: i32,
) {
    let mark = begin_packet(out, id::PLAYER_ACTION);
    put_var_u64(out, runtime_id);
    put_var_i32(out, action);
    put_ublock_pos(out, pos);
    put_ublock_pos(out, [0, 0, 0]);
    put_var_i32(out, face);
    end_packet(out, mark);
}

/// Appends `Respawn` with state "client ready".
pub fn encode_respawn_ready(out: &mut Vec<u8>, runtime_id: u64) {
    let mark = begin_packet(out, id::RESPAWN);
    put_vec3(out, [0.0; 3]);
    out.push(2);
    put_var_u64(out, runtime_id);
    end_packet(out, mark);
}

/// Appends `ContainerClose`.
pub fn encode_container_close(out: &mut Vec<u8>, window: u8, container_type: i8) {
    let mark = begin_packet(out, id::CONTAINER_CLOSE);
    out.push(window);
    out.push(container_type as u8);
    out.push(0);
    end_packet(out, mark);
}

/// Appends `Interact` "open inventory".
pub fn encode_open_inventory(out: &mut Vec<u8>, runtime_id: u64) {
    let mark = begin_packet(out, id::INTERACT);
    out.push(6);
    put_var_u64(out, runtime_id);
    out.push(0);
    end_packet(out, mark);
}

#[cfg(test)]
mod tests {
    use super::*;
    use torchflower_protocol_core::wire::iter_packets;

    #[test]
    fn chat_round_trip_both_layouts() {
        for protocol in [766, 898, 975] {
            let mut out = Vec::new();
            encode_chat(&mut out, protocol, "Bot", "hello", "123");
            let pkt = iter_packets(&out).next().unwrap().unwrap();
            let msg = TextMessage::decode(pkt.payload, protocol).unwrap();
            assert_eq!(msg.kind, text_type::CHAT);
            assert_eq!(msg.source, "Bot");
            assert_eq!(msg.message, "hello");
            assert_eq!(msg.xuid, "123");
        }
    }

    #[test]
    fn auth_input_layout() {
        let mut out = Vec::new();
        let actions = [BlockAction {
            action: block_action::START_BREAK,
            pos: [1, 64, -3],
            face: 1,
        }];
        encode_auth_input(
            &mut out,
            &AuthInput {
                pitch: 10.0,
                yaw: 20.0,
                position: [0.5, 65.62, 0.5],
                move_vector: [0.0, 1.0],
                head_yaw: 20.0,
                flags: torchflower_physics::input_flags::UP,
                tick: 7,
                delta: [0.0, 0.0, 0.2],
                camera: [0.0, 0.0, 1.0],
                raw_move_vector: [0.0, 1.0],
                block_actions: &actions,
                item_stack_request: None,
            },
        );
        let pkt = iter_packets(&out).next().unwrap().unwrap();
        assert_eq!(pkt.id, id::PLAYER_AUTH_INPUT);
        let mut r = WireReader::new(pkt.payload);
        assert_eq!(r.f32_le().unwrap(), 10.0);
        assert_eq!(r.f32_le().unwrap(), 20.0);
        assert_eq!(r.vec3().unwrap(), [0.5, 65.62, 0.5]);
        assert_eq!(r.vec2().unwrap(), [0.0, 1.0]);
        assert_eq!(r.f32_le().unwrap(), 20.0);
        // Flags varint: UP | PERFORM_BLOCK_ACTIONS.
        let mut flags: u128 = 0;
        let mut shift = 0;
        loop {
            let b = r.u8().unwrap();
            flags |= ((b & 0x7f) as u128) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        assert_eq!(
            flags,
            torchflower_physics::input_flags::UP
                | torchflower_physics::input_flags::PERFORM_BLOCK_ACTIONS
        );
        assert_eq!(r.var_u32().unwrap(), 1);
        assert_eq!(r.var_u32().unwrap(), 0);
        assert_eq!(r.var_u32().unwrap(), 1);
        r.vec2().unwrap();
        assert_eq!(r.var_u64().unwrap(), 7);
        r.vec3().unwrap();
        assert_eq!(r.var_i32().unwrap(), 1);
        assert_eq!(r.var_i32().unwrap(), 0);
        assert_eq!(r.block_pos().unwrap(), [1, 64, -3]);
        assert_eq!(r.var_i32().unwrap(), 1);
        r.vec2().unwrap();
        r.vec3().unwrap();
        r.vec2().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn decodes_move_actor_delta() {
        let mut p = Vec::new();
        put_var_u64(&mut p, 9);
        p.extend_from_slice(&(DELTA_X | DELTA_Z | DELTA_ROT_Y).to_le_bytes());
        put_f32(&mut p, 1.5);
        put_f32(&mut p, -2.0);
        p.push(64);
        match Inbound::decode(id::MOVE_ACTOR_DELTA, &p, 898).unwrap() {
            Inbound::MoveActor {
                runtime_id,
                position,
                yaw,
                ..
            } => {
                assert_eq!(runtime_id, 9);
                assert_eq!(position.unwrap(), [Some(1.5), None, Some(-2.0)]);
                assert_eq!(yaw, Some(90.0));
            }
            other => panic!("{other:?}"),
        }
    }
}
