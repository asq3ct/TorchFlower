//! Zero-copy decoding of the game packets the bot consumes, and encoders for
//! the packets it sends.
//!
//! Layouts follow gophertunnel's definitions for protocols 766 (1.21.50)
//! through 1001 (1.26.30). Every layout change in that range that affects a
//! packet used here is gated on the negotiated protocol; see [`version`].

use torchflower_inventory::{ItemFormat, ItemStack};
use torchflower_protocol_core::wire::{
    begin_packet, end_packet, put_block_pos, put_f32, put_string, put_ublock_pos, put_var_i32,
    put_var_u128, put_var_u32, put_var_u64, put_vec2, put_vec3, NbtFlavor, WireError, WireReader,
};

/// Packet ids.
pub mod id {
    /// `PlayStatus` (0x02).
    pub const PLAY_STATUS: u32 = 0x02;
    /// `Disconnect` (0x05).
    pub const DISCONNECT: u32 = 0x05;
    /// `ResourcePacksInfo` (0x06).
    pub const RESOURCE_PACKS_INFO: u32 = 0x06;
    /// `ResourcePackStack` (0x07).
    pub const RESOURCE_PACK_STACK: u32 = 0x07;
    /// `Text` (0x09).
    pub const TEXT: u32 = 0x09;
    /// `SetTime` (0x0a).
    pub const SET_TIME: u32 = 0x0a;
    /// `StartGame` (0x0b).
    pub const START_GAME: u32 = 0x0b;
    /// `AddPlayer` (0x0c).
    pub const ADD_PLAYER: u32 = 0x0c;
    /// `AddActor` (0x0d).
    pub const ADD_ACTOR: u32 = 0x0d;
    /// `RemoveActor` (0x0e).
    pub const REMOVE_ACTOR: u32 = 0x0e;
    /// `AddItemActor` (0x0f).
    pub const ADD_ITEM_ACTOR: u32 = 0x0f;
    /// `TakeItemActor` (0x11).
    pub const TAKE_ITEM_ACTOR: u32 = 0x11;
    /// `MoveActorAbsolute` (0x12).
    pub const MOVE_ACTOR_ABSOLUTE: u32 = 0x12;
    /// `MovePlayer` (0x13).
    pub const MOVE_PLAYER: u32 = 0x13;
    /// `UpdateBlock` (0x15).
    pub const UPDATE_BLOCK: u32 = 0x15;
    /// `ActorEvent` (0x1b).
    pub const ACTOR_EVENT: u32 = 0x1b;
    /// `UpdateAttributes` (0x1d).
    pub const UPDATE_ATTRIBUTES: u32 = 0x1d;
    /// `InventoryTransaction` (0x1e).
    pub const INVENTORY_TRANSACTION: u32 = 0x1e;
    /// `MobEquipment` (0x1f).
    pub const MOB_EQUIPMENT: u32 = 0x1f;
    /// `Interact` (0x21).
    pub const INTERACT: u32 = 0x21;
    /// `PlayerAction` (0x24).
    pub const PLAYER_ACTION: u32 = 0x24;
    /// `SetActorMotion` (0x28).
    pub const SET_ACTOR_MOTION: u32 = 0x28;
    /// `SetHealth` (0x2a).
    pub const SET_HEALTH: u32 = 0x2a;
    /// `Animate` (0x2c).
    pub const ANIMATE: u32 = 0x2c;
    /// `Respawn` (0x2d).
    pub const RESPAWN: u32 = 0x2d;
    /// `ContainerOpen` (0x2e).
    pub const CONTAINER_OPEN: u32 = 0x2e;
    /// `ContainerClose` (0x2f).
    pub const CONTAINER_CLOSE: u32 = 0x2f;
    /// `PlayerHotBar` (0x30).
    pub const PLAYER_HOTBAR: u32 = 0x30;
    /// `InventoryContent` (0x31).
    pub const INVENTORY_CONTENT: u32 = 0x31;
    /// `InventorySlot` (0x32).
    pub const INVENTORY_SLOT: u32 = 0x32;
    /// `CraftingData` (0x34).
    pub const CRAFTING_DATA: u32 = 0x34;
    /// `LevelChunk` (0x3a).
    pub const LEVEL_CHUNK: u32 = 0x3a;
    /// `ChangeDimension` (0x3d).
    pub const CHANGE_DIMENSION: u32 = 0x3d;
    /// `ChunkRadiusUpdated` (0x46).
    pub const CHUNK_RADIUS_UPDATED: u32 = 0x46;
    /// `CommandRequest` (0x4d).
    pub const COMMAND_REQUEST: u32 = 0x4d;
    /// `ModalFormRequest` (0x64).
    pub const MODAL_FORM_REQUEST: u32 = 0x64;
    /// `ModalFormResponse` (0x65).
    pub const MODAL_FORM_RESPONSE: u32 = 0x65;
    /// `MoveActorDelta` (0x6f).
    pub const MOVE_ACTOR_DELTA: u32 = 0x6f;
    /// `NetworkStackLatency` (0x73).
    pub const NETWORK_STACK_LATENCY: u32 = 0x73;
    /// `NetworkChunkPublisherUpdate` (0x79).
    pub const NETWORK_CHUNK_PUBLISHER_UPDATE: u32 = 0x79;
    /// `PlayerAuthInput` (0x90).
    pub const PLAYER_AUTH_INPUT: u32 = 0x90;
    /// `ItemStackRequest` (0x93).
    pub const ITEM_STACK_REQUEST: u32 = 0x93;
    /// `ItemStackResponse` (0x94).
    pub const ITEM_STACK_RESPONSE: u32 = 0x94;
    /// `CorrectPlayerMovePrediction` (0xa1).
    pub const CORRECT_PLAYER_MOVE_PREDICTION: u32 = 0xa1;
    /// `ItemRegistry` (0xa2).
    pub const ITEM_REGISTRY: u32 = 0xa2;
    /// `SubChunk` (0xae).
    pub const SUB_CHUNK: u32 = 0xae;
    /// `ClientBoundCloseForm` (server closes any open form; no payload).
    pub const CLIENT_BOUND_CLOSE_FORM: u32 = 0x136;
}

/// Protocol versions at which packet layouts used by the bot change.
pub mod version {
    /// 1.21.60: the item table moved from `StartGame` to `ItemRegistry`,
    /// and item entries gained a version and NBT data.
    pub const ITEM_REGISTRY_PACKET: i32 = 776;
    /// 1.21.90: `StartGame` drops the movement type and gains an owner id.
    pub const START_GAME_NO_MOVEMENT_TYPE: i32 = 818;
    /// 1.21.100: `CorrectPlayerMovePrediction` always carries a rotation.
    pub const CORRECTION_ALWAYS_ROTATION: i32 = 827;
    /// 1.21.111: `StartGame`'s experimental-gameplay flag is a plain bool
    /// (optional before this and again from 1.26.20).
    pub const START_GAME_PLAIN_EXPERIMENTAL_FLAG: i32 = 844;
    /// 1.21.120: `Animate` data float is always present.
    pub const ANIMATE_DATA_FIELD: i32 = 859;
    /// 1.21.130: `Text` category byte (with constant strings), string
    /// `CommandRequest` version, `Animate` swing source, optional
    /// `Interact` position.
    pub const TEXT_CATEGORY: i32 = 898;
    /// 1.26.0: the constant strings after the `Text` category are dropped.
    pub const TEXT_NO_CONSTANTS: i32 = 924;
    /// 1.26.10: signed block positions (`UpdateBlock`, `ContainerOpen`,
    /// `PlayerAction`, use-item transactions) and the use-item cooldown byte.
    pub const SIGNED_BLOCK_POS: i32 = 944;
    /// 1.26.20: compact item encoding in `MobEquipment` and `InventorySlot`.
    pub const COMPACT_EQUIPMENT_ITEMS: i32 = 975;
    /// 1.26.30: compact items and new integer types in inventory
    /// transactions, presence flags in the transaction header.
    pub const TRANSACTION_V2: i32 = 1001;
}

/// First protocol using the categorised `Text` layout (1.21.130).
pub const TEXT_CATEGORY_PROTOCOL: i32 = version::TEXT_CATEGORY;

fn put_pos(out: &mut Vec<u8>, protocol: i32, pos: [i32; 3]) {
    if protocol >= version::SIGNED_BLOCK_POS {
        put_block_pos(out, pos);
    } else {
        put_ublock_pos(out, pos);
    }
}

fn read_pos(r: &mut WireReader<'_>, protocol: i32) -> Result<[i32; 3], WireError> {
    if protocol >= version::SIGNED_BLOCK_POS {
        r.block_pos()
    } else {
        r.ublock_pos()
    }
}

fn equipment_item_format(protocol: i32) -> ItemFormat {
    if protocol >= version::COMPACT_EQUIPMENT_ITEMS {
        ItemFormat::Compact
    } else {
        ItemFormat::Legacy
    }
}

fn transaction_item_format(protocol: i32) -> ItemFormat {
    if protocol >= version::TRANSACTION_V2 {
        ItemFormat::Compact
    } else {
        ItemFormat::Legacy
    }
}

/// PlayStatus values.
pub mod play_status {
    /// Login accepted.
    pub const LOGIN_SUCCESS: i32 = 0;
    /// The player may spawn.
    pub const PLAYER_SPAWN: i32 = 3;
}

/// Decoded chat/system message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMessage {
    /// Text type (see [`text_type`]).
    pub kind: u8,
    /// Sender name (chat, whisper, announcement).
    pub source: String,
    /// Message text (a translation key for translated messages).
    pub message: String,
    /// Translation parameters.
    pub parameters: Vec<String>,
    /// Sender XUID (may be empty).
    pub xuid: String,
}

/// Text types.
pub mod text_type {
    /// `Raw` text.
    pub const RAW: u8 = 0;
    /// `Chat` text.
    pub const CHAT: u8 = 1;
    /// `Translation` text.
    pub const TRANSLATION: u8 = 2;
    /// `Popup` text.
    pub const POPUP: u8 = 3;
    /// `JukeboxPopup` text.
    pub const JUKEBOX_POPUP: u8 = 4;
    /// `Tip` text.
    pub const TIP: u8 = 5;
    /// `System` text.
    pub const SYSTEM: u8 = 6;
    /// `Whisper` text.
    pub const WHISPER: u8 = 7;
    /// `Announcement` text.
    pub const ANNOUNCEMENT: u8 = 8;
    /// `ObjectWhisper` text.
    pub const OBJECT_WHISPER: u8 = 9;
    /// `Object` text.
    pub const OBJECT: u8 = 10;
    /// `ObjectAnnouncement` text.
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
    ///
    /// * `< 898`: `type, needs_translation, body…`
    /// * `898`: `needs_translation, category, constant strings, type, body…`
    /// * `≥ 924`: `needs_translation, category, type, body…`
    pub fn decode(payload: &[u8], protocol: i32) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        let kind;
        if protocol >= version::TEXT_CATEGORY {
            r.bool()?;
            let category = r.u8()?;
            if protocol < version::TEXT_NO_CONSTANTS {
                let consts = match category {
                    0 => 6,
                    _ => 3,
                };
                for _ in 0..consts {
                    r.string()?;
                }
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
    if protocol >= version::TEXT_CATEGORY {
        out.push(0); // needs translation
        out.push(1); // category: authored message
        if protocol < version::TEXT_NO_CONSTANTS {
            for c in ["chat", "whisper", "announcement"] {
                put_string(out, c);
            }
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
    if protocol >= version::TEXT_CATEGORY {
        out.push(0); // no filtered message
    } else {
        put_string(out, "");
    }
    end_packet(out, mark);
}

/// Appends a `CommandRequest` for `command` (leading `/` optional).
pub fn encode_command(
    out: &mut Vec<u8>,
    protocol: i32,
    command: &str,
    uuid: [u8; 16],
    request_id: &str,
) {
    let mark = begin_packet(out, id::COMMAND_REQUEST);
    if command.starts_with('/') {
        put_string(out, command);
    } else {
        put_string(out, &format!("/{command}"));
    }
    // CommandOrigin. Up to 1.21.124 the origin is a varuint enum (0 =
    // player) and the unique id is only written for dev-console and test
    // origins. From 1.21.130 it is a string and the id is always present.
    if protocol >= version::TEXT_CATEGORY {
        put_string(out, "player");
        put_uuid(out, uuid);
        put_string(out, request_id);
        out.extend_from_slice(&0i64.to_le_bytes());
    } else {
        put_var_u32(out, 0); // origin: player
        put_uuid(out, uuid);
        put_string(out, request_id);
    }
    out.push(0); // internal
                 // Command version: a string from 1.21.130 (the engine's validated
                 // client sends "52"), a zig-zag varint before that.
    if protocol >= version::TEXT_CATEGORY {
        put_string(out, "52");
    } else {
        put_var_i32(out, 52);
    }
    end_packet(out, mark);
}

/// Appends a UUID in Bedrock's wire order: the two 64-bit halves of the
/// big-endian UUID, each written little-endian.
pub fn put_uuid(out: &mut Vec<u8>, uuid: [u8; 16]) {
    out.extend(uuid[..8].iter().rev());
    out.extend(uuid[8..].iter().rev());
}

/// Fields of `StartGame` that the bot needs.
#[derive(Debug, Clone, PartialEq)]
pub struct StartGameInfo<'a> {
    /// The player's unique entity id.
    pub unique_id: i64,
    /// The player's runtime entity id.
    pub runtime_id: u64,
    /// Player game mode.
    pub game_mode: i32,
    /// Eye position.
    pub position: [f32; 3],
    /// Pitch in degrees.
    pub pitch: f32,
    /// Yaw in degrees.
    pub yaw: f32,
    /// Dimension id (0 overworld, 1 nether, 2 end).
    pub dimension: i32,
    /// Fields from the rest of the packet, or `None` if its layout did not
    /// match the negotiated protocol.
    pub policy: Option<StartGamePolicy<'a>>,
}

/// Server policy fields that live in the tail of `StartGame`.
#[derive(Debug, Clone, PartialEq)]
pub struct StartGamePolicy<'a> {
    /// The server runs block breaking itself.
    pub server_authoritative_block_breaking: bool,
    /// Block runtime ids are FNV-1a hashes rather than palette indices.
    pub block_network_ids_are_hashes: bool,
    /// The server runs inventory transactions itself.
    pub server_authoritative_inventory: bool,
    /// Number of custom (add-on) block states the server announced.
    pub custom_block_count: u32,
    /// Item table `(name, network id)`. Only sent in `StartGame` before
    /// protocol 776; later versions use the `ItemRegistry` packet.
    pub items: Vec<(&'a str, i16)>,
}

impl<'a> StartGameInfo<'a> {
    /// Decodes a `StartGame` payload.
    ///
    /// The leading fields are the same in every supported protocol. The rest
    /// of the packet has changed repeatedly, so it is walked with the layout
    /// for `protocol`; if that walk fails, [`StartGameInfo::policy`] is
    /// `None` and the caller must fall back (the bot then detects the
    /// runtime-id mode from chunk data).
    pub fn decode(payload: &'a [u8], protocol: i32) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        let unique_id = r.var_i64()?;
        let runtime_id = r.var_u64()?;
        let game_mode = r.var_i32()?;
        let position = r.vec3()?;
        let pitch = r.f32_le()?;
        let yaw = r.f32_le()?;
        r.skip(8, "world seed")?;
        r.skip(2, "spawn biome type")?;
        r.string()?; // user defined biome name
        let dimension = r.var_i32()?;
        let policy = read_start_game_policy(&mut r, protocol).ok();
        Ok(Self {
            unique_id,
            runtime_id,
            game_mode,
            position,
            pitch,
            yaw,
            dimension,
            policy,
        })
    }
}

/// Whether `StartGame`'s force-experimental-gameplay flag is wrapped in an
/// optional. It is in 1.21.50–1.21.100 and again from 1.26.20; between those it
/// is a plain bool.
fn experimental_flag_is_optional(protocol: i32) -> bool {
    !(version::START_GAME_PLAIN_EXPERIMENTAL_FLAG..version::COMPACT_EQUIPMENT_ITEMS)
        .contains(&protocol)
}

/// Walks the tail of `StartGame` (from just after the dimension) for
/// `protocol`, stopping once the runtime-id mode has been read.
fn read_start_game_policy<'a>(
    r: &mut WireReader<'a>,
    protocol: i32,
) -> Result<StartGamePolicy<'a>, WireError> {
    r.var_i32()?; // generator
    r.var_i32()?; // world game mode
    r.bool()?; // hardcore
    r.var_i32()?; // difficulty
    read_pos(r, protocol)?; // world spawn
    r.bool()?; // achievements disabled
    r.var_i32()?; // editor world type
    r.bool()?; // created in editor
    r.bool()?; // exported from editor
    r.var_i32()?; // day cycle lock time
    r.var_i32()?; // education edition offer
    r.bool()?; // education features enabled
    r.string()?; // education product id
    r.f32_le()?; // rain level
    r.f32_le()?; // lightning level
    r.bool()?; // confirmed platform locked content
    r.bool()?; // multi player game
    r.bool()?; // LAN broadcast enabled
    r.var_i32()?; // XBL broadcast mode
    r.var_i32()?; // platform broadcast mode
    r.bool()?; // commands enabled
    r.bool()?; // texture pack required
    let rules = r.var_u32()?;
    for _ in 0..rules.min(4096) {
        r.string()?; // name
        r.bool()?; // can be modified by player
        match r.var_u32()? {
            1 => {
                r.bool()?;
            }
            2 => {
                r.var_u32()?;
            }
            3 => {
                r.f32_le()?;
            }
            _ => return Err(r.err("game rule type")),
        }
    }
    let experiments = r.u32_le()?;
    for _ in 0..experiments.min(4096) {
        r.string()?; // name
        r.bool()?; // enabled
    }
    r.bool()?; // experiments previously toggled
    r.bool()?; // bonus chest enabled
    r.bool()?; // start with map enabled
    r.var_i32()?; // player permissions
    r.skip(4, "server chunk tick radius")?;
    for _ in 0..10 {
        r.bool()?; // locked packs … emote chat muted
    }
    r.string()?; // base game version
    r.skip(8, "limited world size")?;
    r.bool()?; // new nether
    r.string()?; // education resource button name
    r.string()?; // education resource link
    if experimental_flag_is_optional(protocol) {
        if r.bool()? {
            r.bool()?;
        }
    } else {
        r.bool()?;
    }
    r.u8()?; // chat restriction level
    r.bool()?; // disable player interactions
    if protocol >= version::TRANSACTION_V2 {
        r.var_i32()?; // server editor connection policy
        r.bool()?; // allow anonymous block drops in editor worlds
    }
    if protocol < version::TEXT_NO_CONSTANTS {
        r.string()?; // server id
        r.string()?; // world id
        r.string()?; // scenario id
        if protocol >= version::START_GAME_NO_MOVEMENT_TYPE {
            r.string()?; // owner id
        }
    }
    r.string()?; // level id
    r.string()?; // world name
    r.string()?; // template content identity
    r.bool()?; // trial
    if protocol < version::START_GAME_NO_MOVEMENT_TYPE {
        r.var_i32()?; // movement type (removed in 1.21.90)
    }
    r.var_i32()?; // rewind history size
    let server_authoritative_block_breaking = r.bool()?;
    r.skip(8, "world time")?;
    r.var_i32()?; // enchantment seed
    let custom_block_count = r.var_u32()?;
    for _ in 0..custom_block_count.min(1 << 16) {
        r.string()?; // block name
        r.skip_nbt(NbtFlavor::Network)?; // block properties
    }
    let mut items = Vec::new();
    if protocol < version::ITEM_REGISTRY_PACKET {
        let count = r.var_u32()?.min(8192);
        items.reserve(count as usize);
        for _ in 0..count {
            let name = r.string()?;
            let network_id = r.i16_le()?;
            r.bool()?; // component based
            items.push((name, network_id));
        }
    }
    r.string()?; // multiplayer correlation id
    let server_authoritative_inventory = r.bool()?;
    r.string()?; // game version
    r.skip_nbt(NbtFlavor::Network)?; // property data
    r.skip(8, "block state checksum")?;
    r.skip(16, "world template id")?;
    r.bool()?; // client side generation
    let block_network_ids_are_hashes = r.bool()?;
    Ok(StartGamePolicy {
        server_authoritative_block_breaking,
        block_network_ids_are_hashes,
        server_authoritative_inventory,
        custom_block_count,
        items,
    })
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
    /// Unique entity id (missing if the `AddPlayer` tail failed to parse).
    pub unique_id: Option<i64>,
    /// Runtime entity id.
    pub runtime_id: u64,
    /// Entity identifier, e.g. `minecraft:zombie`.
    pub kind: &'a str,
    /// Player name, for players.
    pub username: Option<&'a str>,
    /// Position (eye position for players, feet for other entities).
    pub position: [f32; 3],
    /// Velocity in blocks per tick.
    pub velocity: [f32; 3],
    /// Pitch in degrees.
    pub pitch: f32,
    /// Yaw in degrees.
    pub yaw: f32,
    /// The item of an item entity, or a player's held item.
    pub item: Option<ItemStack>,
}

/// Decoded inbound packet.
///
/// Packets with large or rarely needed bodies are passed through as borrowed
/// payload slices and decoded by the subsystem that owns them.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound<'a> {
    /// `PlayStatus` (see [`play_status`]).
    PlayStatus(i32),
    /// `Disconnect` with the kick message.
    Disconnect(String),
    /// `ResourcePacksInfo`.
    ResourcePacksInfo,
    /// `ResourcePackStack`.
    ResourcePackStack,
    /// `Text`.
    Text(TextMessage),
    /// `SetTime`: world time.
    SetTime(i32),
    /// `StartGame`.
    StartGame(StartGameInfo<'a>),
    /// `AddPlayer`, `AddActor` or `AddItemActor`.
    Spawn(SpawnInfo<'a>),
    /// `RemoveActor`: unique id.
    RemoveActor(i64),
    /// `TakeItemActor`: an item entity was picked up.
    TakeItemActor {
        /// Runtime id of the item entity.
        item: u64,
        /// Runtime id of the entity that picked it up.
        taker: u64,
    },
    /// `MoveActorAbsolute` or `MoveActorDelta`.
    MoveActor {
        /// Runtime id of the entity.
        runtime_id: u64,
        /// New coordinates (`None` components are unchanged).
        position: Option<[Option<f32>; 3]>,
        /// New yaw, if sent.
        yaw: Option<f32>,
        /// New pitch, if sent.
        pitch: Option<f32>,
        /// Whether the entity is on the ground.
        on_ground: bool,
        /// Whether this is a teleport.
        teleport: bool,
    },
    /// `MovePlayer`.
    MovePlayer(MovePlayer),
    /// `CorrectPlayerMovePrediction`.
    CorrectMove(MoveCorrection),
    /// `SetActorMotion`.
    SetActorMotion {
        /// Runtime id of the entity.
        runtime_id: u64,
        /// New velocity in blocks per tick.
        velocity: [f32; 3],
    },
    /// `SetHealth`.
    SetHealth(i32),
    /// `UpdateAttributes`.
    Attributes {
        /// Runtime id of the entity.
        runtime_id: u64,
        /// `(name, value, max)` per attribute.
        values: Vec<(&'a str, f32, f32)>,
    },
    /// `Respawn`.
    Respawn {
        /// Respawn eye position.
        position: [f32; 3],
        /// 0 searching, 1 server ready, 2 client ready.
        state: u8,
        /// Runtime id of the player.
        runtime_id: u64,
    },
    /// `UpdateBlock`.
    UpdateBlock {
        /// Block position.
        pos: [i32; 3],
        /// New runtime id.
        runtime_id: u32,
        /// Storage layer (0 blocks, 1 water-logging).
        layer: u32,
    },
    /// `LevelChunk` payload (decoded by `torchflower-world`).
    LevelChunk(&'a [u8]),
    /// `SubChunk` payload (decoded by `torchflower-world`).
    SubChunk(&'a [u8]),
    /// `InventoryContent` payload (decoded by `torchflower-inventory`).
    InventoryContent(&'a [u8]),
    /// `InventorySlot` payload (decoded by `torchflower-inventory`).
    InventorySlot(&'a [u8]),
    /// `ContainerOpen` payload.
    ContainerOpen(&'a [u8]),
    /// `ContainerClose` payload.
    ContainerClose(&'a [u8]),
    /// `ItemStackResponse` payload.
    ItemStackResponse(&'a [u8]),
    /// `ItemRegistry` payload. Only produced from protocol
    /// [`version::ITEM_REGISTRY_PACKET`] upwards; older versions send the item
    /// table inside `StartGame` and reuse this packet id for `ItemComponent`.
    ItemRegistry(&'a [u8]),
    /// `CraftingData` payload.
    CraftingData(&'a [u8]),
    /// `NetworkStackLatency`.
    NetworkStackLatency {
        /// Server timestamp to echo.
        timestamp: i64,
        /// Whether the server expects an answer.
        needs_response: bool,
    },
    /// `ChangeDimension`.
    ChangeDimension {
        /// New dimension id.
        dimension: i32,
        /// Eye position in the new dimension.
        position: [f32; 3],
    },
    /// `MobEquipment`.
    MobEquipment {
        /// Runtime id of the entity.
        runtime_id: u64,
        /// Item now held.
        item: ItemStack,
        /// Selected hotbar slot.
        hotbar_slot: u8,
        /// Window id (0 = inventory).
        window: u8,
    },
    /// `PlayerHotbar`.
    PlayerHotbar {
        /// Hotbar slot.
        slot: u32,
        /// Window id.
        window: u8,
        /// Whether the client should select the slot.
        select: bool,
    },
    /// `ChunkRadiusUpdated`: the chunk radius the server settled on.
    ChunkRadiusUpdated(i32),
    /// `ModalFormRequest`.
    ModalForm {
        /// Id to answer with.
        form_id: u32,
        /// Form JSON.
        data: &'a str,
    },
    /// `ClientBoundCloseForm`: close all open forms.
    CloseForm,
    /// `ActorEvent`.
    ActorEvent {
        /// Runtime id of the entity.
        runtime_id: u64,
        /// Event id (see [`actor_event`]).
        event: u8,
        /// Event data.
        data: i32,
    },
    /// Any packet the bot does not use (its id).
    Other(u32),
}

/// `MovePlayer` modes.
pub mod move_mode {
    /// Regular movement.
    pub const NORMAL: u8 = 0;
    /// Position reset by the server.
    pub const RESET: u8 = 1;
    /// Teleport.
    pub const TELEPORT: u8 = 2;
    /// Rotation-only update.
    pub const ROTATION: u8 = 3;
}

/// Decoded `MovePlayer` (0x13).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovePlayer {
    /// Runtime id of the player that moved.
    pub runtime_id: u64,
    /// Eye position.
    pub position: [f32; 3],
    /// Pitch in degrees.
    pub pitch: f32,
    /// Yaw in degrees.
    pub yaw: f32,
    /// Head yaw in degrees.
    pub head_yaw: f32,
    /// One of [`move_mode`].
    pub mode: u8,
    /// Whether the player is on the ground.
    pub on_ground: bool,
    /// Runtime id of the entity being ridden (0 if none).
    pub ridden_runtime_id: u64,
    /// `(cause, source entity type)` for teleports.
    pub teleport: Option<(i32, i32)>,
    /// Server tick the move applies to (0 when unknown).
    pub tick: u64,
}

/// Prediction type of a `CorrectPlayerMovePrediction`.
pub mod prediction_type {
    /// Correction of the player.
    pub const PLAYER: u8 = 0;
    /// Correction of the ridden vehicle.
    pub const VEHICLE: u8 = 1;
}

/// Decoded `CorrectPlayerMovePrediction` (0xa1): the server-authoritative
/// state of the player at `tick`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveCorrection {
    /// One of [`prediction_type`].
    pub prediction_type: u8,
    /// Authoritative eye position.
    pub position: [f32; 3],
    /// Authoritative velocity (blocks per tick).
    pub velocity: [f32; 3],
    /// `(pitch, yaw)` (always present from protocol 827, otherwise only
    /// for vehicle predictions).
    pub rotation: Option<[f32; 2]>,
    /// Authoritative on-ground state.
    pub on_ground: bool,
    /// The client input tick that was corrected.
    pub tick: u64,
}

impl MoveCorrection {
    /// Decodes the payload for `protocol`.
    pub fn decode(p: &[u8], protocol: i32) -> Result<Self, WireError> {
        let mut r = WireReader::new(p);
        let prediction_type = r.u8()?;
        let position = r.vec3()?;
        let velocity = r.vec3()?;
        let mut rotation = None;
        if protocol >= version::CORRECTION_ALWAYS_ROTATION
            || prediction_type == prediction_type::VEHICLE
        {
            rotation = Some(r.vec2()?);
            if r.bool()? {
                r.f32_le()?; // vehicle angular velocity
            }
        }
        let on_ground = r.bool()?;
        let tick = r.var_u64()?;
        Ok(Self {
            prediction_type,
            position,
            velocity,
            rotation,
            on_ground,
            tick,
        })
    }
}

impl MovePlayer {
    /// Decodes the payload.
    pub fn decode(p: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(p);
        let runtime_id = r.var_u64()?;
        let position = r.vec3()?;
        let pitch = r.f32_le()?;
        let yaw = r.f32_le()?;
        let head_yaw = r.f32_le()?;
        let mode = r.u8()?;
        let on_ground = r.bool()?;
        let ridden_runtime_id = r.var_u64()?;
        let teleport = if mode == move_mode::TELEPORT {
            Some((r.i32_le()?, r.i32_le()?))
        } else {
            None
        };
        let tick = r.var_u64().unwrap_or(0);
        Ok(Self {
            runtime_id,
            position,
            pitch,
            yaw,
            head_yaw,
            mode,
            on_ground,
            ridden_runtime_id,
            teleport,
            tick,
        })
    }
}

/// `ActorEvent` ids used by the bot.
pub mod actor_event {
    /// The entity was hurt.
    pub const HURT: u8 = 2;
    /// The entity died.
    pub const DEATH: u8 = 3;
    /// Eating particles/animation (`Feed` / `EATING_ITEM`). Sent by the
    /// server while an entity eats; `data` is `(network_id << 16) | meta`.
    pub const FEED: u8 = 57;
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
            id::START_GAME => Inbound::StartGame(StartGameInfo::decode(p, protocol)?),
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
            id::MOVE_PLAYER => Inbound::MovePlayer(MovePlayer::decode(p)?),
            id::CORRECT_PLAYER_MOVE_PREDICTION => {
                Inbound::CorrectMove(MoveCorrection::decode(p, protocol)?)
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
                pos: read_pos(&mut r, protocol)?,
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
            // Before 1.21.60 this id carries `ItemComponent` (a list of
            // name + NBT pairs) and the item table lives in `StartGame`, so
            // decoding it as an item registry would read garbage.
            id::ITEM_REGISTRY if protocol >= version::ITEM_REGISTRY_PACKET => {
                Inbound::ItemRegistry(p)
            }
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
                item: ItemStack::read_with(&mut r, equipment_item_format(protocol))?,
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
            id::MODAL_FORM_REQUEST => {
                let form_id = r.var_u32()?;
                let raw = r.byte_slice()?;
                let data = std::str::from_utf8(raw).map_err(|_| r.err("form json utf-8"))?;
                Inbound::ModalForm { form_id, data }
            }
            id::CLIENT_BOUND_CLOSE_FORM => Inbound::CloseForm,
            id::ACTOR_EVENT => Inbound::ActorEvent {
                runtime_id: r.var_u64()?,
                event: r.u8()?,
                data: r.var_i32()?,
            },
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
    /// Action id (see [`block_action`]).
    pub action: i32,
    /// Target block.
    pub pos: [i32; 3],
    /// Face id (0 down … 5 east).
    pub face: i32,
}

/// Block action ids.
pub mod block_action {
    /// `StartBreak` player action.
    pub const START_BREAK: i32 = 0;
    /// `AbortBreak` player action.
    pub const ABORT_BREAK: i32 = 1;
    /// `StopBreak` player action.
    pub const STOP_BREAK: i32 = 2;
    /// `Respawn` player action.
    pub const RESPAWN: i32 = 7;
    /// `DimensionChangeDone` player action.
    pub const DIMENSION_CHANGE_DONE: i32 = 14;
    /// `CrackBreak` player action.
    pub const CRACK_BREAK: i32 = 18;
    /// `PredictDestroyBlock` player action.
    pub const PREDICT_DESTROY_BLOCK: i32 = 26;
    /// `ContinueDestroyBlock` player action.
    pub const CONTINUE_DESTROY_BLOCK: i32 = 27;
}

/// `UseItem` transaction data.
#[derive(Debug, Clone, PartialEq)]
pub struct UseItem<'a> {
    /// One of [`use_item_action`].
    pub action: u32,
    /// Clicked block (ignored for click-air).
    pub block_pos: [i32; 3],
    /// Clicked face, or −1 for click-air.
    pub face: i32,
    /// Selected hotbar slot.
    pub hotbar_slot: i32,
    /// The held item, as the client believes it to be.
    pub held: &'a ItemStack,
    /// Player (eye) position.
    pub player_pos: [f32; 3],
    /// Click position relative to the block (0..1).
    pub click_pos: [f32; 3],
    /// Runtime id of the clicked block.
    pub block_runtime_id: u32,
}

/// `UseItem` transaction action types.
pub mod use_item_action {
    /// Use the held item on a block.
    pub const CLICK_BLOCK: u32 = 0;
    /// Use the held item without targeting a block (eating, bows, ...).
    pub const CLICK_AIR: u32 = 1;
    /// Break a block (legacy, non-authoritative breaking).
    pub const BREAK_BLOCK: u32 = 2;
}

/// `ReleaseItem` transaction action types.
pub mod release_item_action {
    /// Stop using the item early.
    pub const RELEASE: u32 = 0;
    /// Finish consuming the item (food, potion).
    pub const CONSUME: u32 = 1;
}

/// Writes the `InventoryTransaction` header up to (and including) the empty
/// action list.
fn put_transaction_header(out: &mut Vec<u8>, protocol: i32, kind: u32) {
    put_var_i32(out, 0); // legacy request id
    if protocol >= version::TRANSACTION_V2 {
        out.push(0); // no legacy slots
        out.push(1); // transaction type present
        put_var_u32(out, kind);
        out.push(1); // actions present
        put_var_u32(out, 0);
    } else {
        put_var_u32(out, kind);
        put_var_u32(out, 0); // actions
    }
}

fn put_use_item(out: &mut Vec<u8>, protocol: i32, u: &UseItem<'_>) {
    let v2 = protocol >= version::TRANSACTION_V2;
    if v2 {
        put_var_i32(out, u.action as i32);
        out.push(1); // trigger: player input
    } else {
        put_var_u32(out, u.action);
        put_var_u32(out, 1);
    }
    put_pos(out, protocol, u.block_pos);
    if v2 {
        out.push(u.face as u8); // -1 (click air) becomes 0xff
    } else {
        put_var_i32(out, u.face);
    }
    put_var_i32(out, u.hotbar_slot);
    u.held.write_with(out, transaction_item_format(protocol));
    put_vec3(out, u.player_pos);
    put_vec3(out, u.click_pos);
    put_var_u32(out, u.block_runtime_id);
    if v2 {
        out.push(1); // client prediction: success
    } else {
        put_var_u32(out, 1);
    }
    if protocol >= version::SIGNED_BLOCK_POS {
        out.push(0); // client cooldown state: off
    }
}

/// Appends a standalone `InventoryTransaction` with use-item data.
pub fn encode_use_item(out: &mut Vec<u8>, protocol: i32, u: &UseItem<'_>) {
    let mark = begin_packet(out, id::INVENTORY_TRANSACTION);
    put_transaction_header(out, protocol, 2);
    put_use_item(out, protocol, u);
    end_packet(out, mark);
}

/// Appends an `InventoryTransaction` releasing or consuming the held item.
pub fn encode_release_item(
    out: &mut Vec<u8>,
    protocol: i32,
    action: u32,
    hotbar_slot: i32,
    held: &ItemStack,
    head_pos: [f32; 3],
) {
    let mark = begin_packet(out, id::INVENTORY_TRANSACTION);
    put_transaction_header(out, protocol, 4);
    if protocol >= version::TRANSACTION_V2 {
        put_var_i32(out, action as i32);
    } else {
        put_var_u32(out, action);
    }
    put_var_i32(out, hotbar_slot);
    held.write_with(out, transaction_item_format(protocol));
    put_vec3(out, head_pos);
    end_packet(out, mark);
}

/// Appends an `InventoryTransaction` attacking (action 1) or interacting
/// (action 0) with an entity.
pub fn encode_use_item_on_entity(
    out: &mut Vec<u8>,
    protocol: i32,
    target: u64,
    action: u32,
    hotbar_slot: i32,
    held: &ItemStack,
    player_pos: [f32; 3],
) {
    let mark = begin_packet(out, id::INVENTORY_TRANSACTION);
    put_transaction_header(out, protocol, 3);
    put_var_u64(out, target);
    if protocol >= version::TRANSACTION_V2 {
        put_var_i32(out, action as i32);
    } else {
        put_var_u32(out, action);
    }
    put_var_i32(out, hotbar_slot);
    held.write_with(out, transaction_item_format(protocol));
    put_vec3(out, player_pos);
    put_vec3(out, [0.0; 3]);
    end_packet(out, mark);
}

/// Per-tick `PlayerAuthInput` fields.
#[derive(Debug, Clone)]
pub struct AuthInput<'a> {
    /// Pitch in degrees.
    pub pitch: f32,
    /// Yaw in degrees.
    pub yaw: f32,
    /// Eye position.
    pub position: [f32; 3],
    /// Normalised `(x, z)` movement input.
    pub move_vector: [f32; 2],
    /// Head yaw in degrees.
    pub head_yaw: f32,
    /// Input flags (see `torchflower_physics::input_flags`).
    pub flags: u128,
    /// Client input tick.
    pub tick: u64,
    /// Position change during this tick.
    pub delta: [f32; 3],
    /// Camera look direction.
    pub camera: [f32; 3],
    /// Unnormalised `(x, z)` movement input.
    pub raw_move_vector: [f32; 2],
    /// Block actions performed this tick.
    pub block_actions: &'a [BlockAction],
    /// Inventory request embedded in the input, if any.
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
pub fn encode_mob_equipment(
    out: &mut Vec<u8>,
    protocol: i32,
    runtime_id: u64,
    item: &ItemStack,
    hotbar_slot: u8,
) {
    let mark = begin_packet(out, id::MOB_EQUIPMENT);
    put_var_u64(out, runtime_id);
    item.write_with(out, equipment_item_format(protocol));
    out.push(hotbar_slot);
    out.push(hotbar_slot);
    out.push(0);
    end_packet(out, mark);
}

/// Appends an arm-swing `Animate`.
pub fn encode_swing(out: &mut Vec<u8>, protocol: i32, runtime_id: u64) {
    let mark = begin_packet(out, id::ANIMATE);
    if protocol >= version::TEXT_CATEGORY {
        out.push(1); // swing arm
        put_var_u64(out, runtime_id);
        put_f32(out, 0.0);
        out.push(0); // no swing source
    } else {
        put_var_i32(out, 1);
        put_var_u64(out, runtime_id);
        if protocol >= version::ANIMATE_DATA_FIELD {
            put_f32(out, 0.0);
        }
    }
    end_packet(out, mark);
}

/// Appends a legacy `PlayerAction`.
pub fn encode_player_action(
    out: &mut Vec<u8>,
    protocol: i32,
    runtime_id: u64,
    action: i32,
    pos: [i32; 3],
    face: i32,
) {
    let mark = begin_packet(out, id::PLAYER_ACTION);
    put_var_u64(out, runtime_id);
    put_var_i32(out, action);
    put_pos(out, protocol, pos);
    put_pos(out, protocol, [0, 0, 0]);
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
pub fn encode_open_inventory(out: &mut Vec<u8>, protocol: i32, runtime_id: u64) {
    let mark = begin_packet(out, id::INTERACT);
    out.push(6);
    put_var_u64(out, runtime_id);
    if protocol >= version::TEXT_CATEGORY {
        out.push(0); // no position
    }
    end_packet(out, mark);
}

/// Why a form was dismissed without a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormCancelReason {
    /// The player closed the form.
    UserClosed = 0,
    /// The client was busy (another UI was open).
    UserBusy = 1,
}

/// Appends a `ModalFormResponse` carrying `response_json`.
///
/// For a simple (button) form the JSON is the button index (`"2"`), for a
/// modal form `"true"`/`"false"`, and for a custom form an array of the
/// element values (`["text", 1, true]`).
pub fn encode_form_response(out: &mut Vec<u8>, form_id: u32, response_json: &str) {
    let mark = begin_packet(out, id::MODAL_FORM_RESPONSE);
    put_var_u32(out, form_id);
    out.push(1); // response present
    put_string(out, response_json);
    out.push(0); // no cancel reason
    end_packet(out, mark);
}

/// Appends a `ModalFormResponse` closing the form without a response.
pub fn encode_form_cancel(out: &mut Vec<u8>, form_id: u32, reason: FormCancelReason) {
    let mark = begin_packet(out, id::MODAL_FORM_RESPONSE);
    put_var_u32(out, form_id);
    out.push(0); // no response
    out.push(1); // cancel reason present
    out.push(reason as u8);
    end_packet(out, mark);
}

#[cfg(test)]
mod tests {
    use super::*;
    use torchflower_protocol_core::wire::{
        iter_packets, put_block_pos, put_ublock_pos, put_var_i64,
    };

    #[test]
    fn chat_round_trip_all_layouts() {
        for protocol in [766, 860, 898, 924, 944, 975, 1001] {
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
    fn text_layout_constants_only_at_898() {
        let mut a = Vec::new();
        encode_chat(&mut a, 898, "B", "m", "");
        let mut b = Vec::new();
        encode_chat(&mut b, 924, "B", "m", "");
        // 898 carries three constant strings ("chat", "whisper", "announcement").
        assert_eq!(a.len() - b.len(), 5 + 8 + 13);
    }

    fn correction_payload(prediction: u8, with_rotation: bool, tick: u64) -> Vec<u8> {
        let mut p = vec![prediction];
        put_vec3(&mut p, [1.5, 65.62, -2.5]);
        put_vec3(&mut p, [0.0, -0.0784, 0.1]);
        if with_rotation {
            put_vec2(&mut p, [10.0, 20.0]);
            p.push(0); // no angular velocity
        }
        p.push(1); // on ground
        put_var_u64(&mut p, tick);
        p
    }

    #[test]
    fn decodes_move_correction_across_versions() {
        // >= 827: rotation always present.
        let c = MoveCorrection::decode(&correction_payload(0, true, 4321), 898).unwrap();
        assert_eq!(c.prediction_type, prediction_type::PLAYER);
        assert_eq!(c.position, [1.5, 65.62, -2.5]);
        assert_eq!(c.velocity, [0.0, -0.0784, 0.1]);
        assert_eq!(c.rotation, Some([10.0, 20.0]));
        assert!(c.on_ground);
        assert_eq!(c.tick, 4321);
        // < 827 player prediction: no rotation on the wire.
        let c = MoveCorrection::decode(&correction_payload(0, false, 77), 766).unwrap();
        assert_eq!(c.rotation, None);
        assert_eq!(c.tick, 77);
        // < 827 vehicle prediction: rotation present.
        let c = MoveCorrection::decode(&correction_payload(1, true, 5), 800).unwrap();
        assert_eq!(c.rotation, Some([10.0, 20.0]));
        assert_eq!(c.tick, 5);
    }

    #[test]
    fn decodes_move_player_teleport() {
        let mut p = Vec::new();
        put_var_u64(&mut p, 7);
        put_vec3(&mut p, [0.5, 70.62, 0.5]);
        for v in [5.0f32, 90.0, 91.0] {
            put_f32(&mut p, v);
        }
        p.push(move_mode::TELEPORT);
        p.push(0);
        put_var_u64(&mut p, 0);
        p.extend_from_slice(&3i32.to_le_bytes());
        p.extend_from_slice(&0i32.to_le_bytes());
        put_var_u64(&mut p, 900);
        let m = MovePlayer::decode(&p).unwrap();
        assert_eq!(m.runtime_id, 7);
        assert_eq!((m.pitch, m.yaw, m.head_yaw), (5.0, 90.0, 91.0));
        assert_eq!(m.mode, move_mode::TELEPORT);
        assert_eq!(m.teleport, Some((3, 0)));
        assert_eq!(m.tick, 900);
    }

    #[test]
    fn modal_forms_round_trip() {
        let mut req = Vec::new();
        put_var_u32(&mut req, 12);
        put_string(&mut req, r#"{"type":"form","title":"Warps","buttons":[]}"#);
        match Inbound::decode(id::MODAL_FORM_REQUEST, &req, 898).unwrap() {
            Inbound::ModalForm { form_id, data } => {
                assert_eq!(form_id, 12);
                assert!(data.contains("Warps"));
            }
            other => panic!("{other:?}"),
        }
        let mut out = Vec::new();
        encode_form_response(&mut out, 12, "2");
        let pkt = iter_packets(&out).next().unwrap().unwrap();
        assert_eq!(pkt.id, id::MODAL_FORM_RESPONSE);
        let mut r = WireReader::new(pkt.payload);
        assert_eq!(r.var_u32().unwrap(), 12);
        assert!(r.bool().unwrap());
        assert_eq!(r.string().unwrap(), "2");
        assert!(!r.bool().unwrap());
        assert_eq!(r.remaining(), 0);

        let mut out = Vec::new();
        encode_form_cancel(&mut out, 12, FormCancelReason::UserClosed);
        let pkt = iter_packets(&out).next().unwrap().unwrap();
        assert_eq!(pkt.payload, &[12, 0, 1, 0]);
        assert_eq!(
            Inbound::decode(id::CLIENT_BOUND_CLOSE_FORM, &[], 1001).unwrap(),
            Inbound::CloseForm
        );
    }

    fn held() -> ItemStack {
        ItemStack {
            network_id: 5,
            count: 1,
            stack_id: 9,
            extra: vec![0u8; 10].into(),
            ..Default::default()
        }
    }

    #[test]
    fn use_item_layout_per_version() {
        let item = held();
        let u = UseItem {
            action: use_item_action::CLICK_BLOCK,
            block_pos: [1, -10, 2],
            face: 1,
            hotbar_slot: 0,
            held: &item,
            player_pos: [0.0; 3],
            click_pos: [0.5, 1.0, 0.5],
            block_runtime_id: 42,
        };
        for protocol in [898, 944, 1001] {
            let mut out = Vec::new();
            encode_use_item(&mut out, protocol, &u);
            let pkt = iter_packets(&out).next().unwrap().unwrap();
            let mut r = WireReader::new(pkt.payload);
            assert_eq!(r.var_i32().unwrap(), 0);
            if protocol >= 1001 {
                assert_eq!([r.u8().unwrap(), r.u8().unwrap()], [0, 1]);
                assert_eq!(r.var_u32().unwrap(), 2);
                assert!(r.bool().unwrap());
                assert_eq!(r.var_u32().unwrap(), 0);
                assert_eq!(r.var_i32().unwrap(), 0);
                assert_eq!(r.u8().unwrap(), 1);
            } else {
                assert_eq!(r.var_u32().unwrap(), 2);
                assert_eq!(r.var_u32().unwrap(), 0);
                assert_eq!(r.var_u32().unwrap(), 0);
                assert_eq!(r.var_u32().unwrap(), 1);
            }
            let pos = if protocol >= 944 {
                r.block_pos().unwrap()
            } else {
                r.ublock_pos().unwrap()
            };
            assert_eq!(pos[0], 1);
            if protocol >= 944 {
                assert_eq!(pos[1], -10, "signed Y from 944");
            }
            let face = if protocol >= 1001 {
                r.u8().unwrap() as i32
            } else {
                r.var_i32().unwrap()
            };
            assert_eq!(face, 1);
            assert_eq!(r.var_i32().unwrap(), 0);
            let format = if protocol >= 1001 {
                ItemFormat::Compact
            } else {
                ItemFormat::Legacy
            };
            assert_eq!(ItemStack::read_with(&mut r, format).unwrap(), item);
            r.vec3().unwrap();
            assert_eq!(r.vec3().unwrap(), [0.5, 1.0, 0.5]);
            assert_eq!(r.var_u32().unwrap(), 42);
            if protocol >= 1001 {
                assert_eq!(r.u8().unwrap(), 1);
            } else {
                assert_eq!(r.var_u32().unwrap(), 1);
            }
            if protocol >= 944 {
                assert_eq!(r.u8().unwrap(), 0, "cooldown byte from 944");
            }
            assert_eq!(r.remaining(), 0, "protocol {protocol}");
        }
    }

    #[test]
    fn release_item_consume_layout() {
        let item = held();
        let mut out = Vec::new();
        encode_release_item(
            &mut out,
            898,
            release_item_action::CONSUME,
            2,
            &item,
            [0.0, 65.62, 0.0],
        );
        let pkt = iter_packets(&out).next().unwrap().unwrap();
        let mut r = WireReader::new(pkt.payload);
        assert_eq!(r.var_i32().unwrap(), 0);
        assert_eq!(r.var_u32().unwrap(), 4, "release item transaction");
        assert_eq!(r.var_u32().unwrap(), 0);
        assert_eq!(r.var_u32().unwrap(), release_item_action::CONSUME);
        assert_eq!(r.var_i32().unwrap(), 2);
        assert_eq!(ItemStack::read_instance(&mut r).unwrap(), item);
        assert_eq!(r.vec3().unwrap(), [0.0, 65.62, 0.0]);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn update_block_signed_position_from_944() {
        let mut p = Vec::new();
        put_block_pos(&mut p, [3, -30, 4]);
        put_var_u32(&mut p, 11);
        put_var_u32(&mut p, 3);
        put_var_u32(&mut p, 0);
        match Inbound::decode(id::UPDATE_BLOCK, &p, 944).unwrap() {
            Inbound::UpdateBlock {
                pos,
                runtime_id,
                layer,
            } => {
                assert_eq!((pos, runtime_id, layer), ([3, -30, 4], 11, 0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mob_equipment_compact_items_from_975() {
        let item = held();
        let mut out = Vec::new();
        encode_mob_equipment(&mut out, 975, 7, &item, 3);
        let pkt = iter_packets(&out).next().unwrap().unwrap();
        match Inbound::decode(id::MOB_EQUIPMENT, pkt.payload, 975).unwrap() {
            Inbound::MobEquipment {
                runtime_id,
                item: got,
                hotbar_slot,
                window,
            } => {
                assert_eq!((runtime_id, hotbar_slot, window), (7, 3, 0));
                assert_eq!(got, item);
            }
            other => panic!("{other:?}"),
        }
        // The legacy decoder must not accept the compact encoding by accident.
        assert_ne!(
            Inbound::decode(id::MOB_EQUIPMENT, pkt.payload, 898).ok(),
            Inbound::decode(id::MOB_EQUIPMENT, pkt.payload, 975).ok()
        );
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

    /// `CommandRequest` byte-for-byte on both sides of the 1.21.130 origin
    /// change: before it the origin is a varuint enum with no unique id and
    /// the version is a zig-zag varint; from it the origin is the string
    /// "player", the unique id is always present and the version is a string.
    #[test]
    fn command_request_origin_layout() {
        let uuid: [u8; 16] = std::array::from_fn(|i| i as u8);
        // Bedrock writes each 64-bit half of the UUID little-endian.
        let wire_uuid = [7u8, 6, 5, 4, 3, 2, 1, 0, 15, 14, 13, 12, 11, 10, 9, 8];

        let mut out = Vec::new();
        encode_command(&mut out, 860, "list", uuid, "req");
        let payload = iter_packets(&out).next().unwrap().unwrap().payload;
        let mut want = Vec::new();
        put_string(&mut want, "/list");
        put_var_u32(&mut want, 0); // origin: player
        want.extend_from_slice(&wire_uuid);
        put_string(&mut want, "req");
        want.push(0); // internal
        put_var_i32(&mut want, 52); // command version
        assert_eq!(payload, &want[..], "pre-1.21.130 layout");

        let mut out = Vec::new();
        encode_command(&mut out, 898, "/list", uuid, "req");
        let payload = iter_packets(&out).next().unwrap().unwrap().payload;
        let mut want = Vec::new();
        put_string(&mut want, "/list"); // already slash-prefixed: kept as is
        put_string(&mut want, "player");
        want.extend_from_slice(&wire_uuid);
        put_string(&mut want, "req");
        want.extend_from_slice(&0i64.to_le_bytes());
        want.push(0); // internal
        put_string(&mut want, "52");
        assert_eq!(payload, &want[..], "1.21.130 layout");
    }

    // -----------------------------------------------------------------
    // StartGame
    // -----------------------------------------------------------------

    /// Builds a `StartGame` payload for `protocol`, following the field order
    /// of the matching Bedrock release. Everything the walker skips is filled
    /// with values that would desynchronise the parse if a field were read
    /// with the wrong width or in the wrong order.
    fn start_game_payload(protocol: i32, items: &[(&str, i16)], hashed: bool) -> Vec<u8> {
        let mut p = Vec::new();
        put_var_i64(&mut p, 42); // entity unique id
        put_var_u64(&mut p, 43); // entity runtime id
        put_var_i32(&mut p, 1); // player game mode
        put_vec3(&mut p, [8.5, 66.62, -4.5]);
        put_f32(&mut p, 11.0); // pitch
        put_f32(&mut p, 22.0); // yaw
        p.extend_from_slice(&7i64.to_le_bytes()); // world seed
        p.extend_from_slice(&1i16.to_le_bytes()); // spawn biome type
        put_string(&mut p, "plains"); // user defined biome
        put_var_i32(&mut p, 0); // dimension

        put_var_i32(&mut p, 2); // generator
        put_var_i32(&mut p, 1); // world game mode
        p.push(0); // hardcore
        put_var_i32(&mut p, 2); // difficulty
        if protocol >= version::SIGNED_BLOCK_POS {
            put_block_pos(&mut p, [-8, 64, -8]);
        } else {
            put_ublock_pos(&mut p, [8, 64, 8]);
        }
        p.push(1); // achievements disabled
        put_var_i32(&mut p, 0); // editor world type
        p.push(0); // created in editor
        p.push(0); // exported from editor
        put_var_i32(&mut p, 0); // day cycle lock time
        put_var_i32(&mut p, 0); // education edition offer
        p.push(0); // education features enabled
        put_string(&mut p, ""); // education product id
        put_f32(&mut p, 0.0); // rain level
        put_f32(&mut p, 0.0); // lightning level
        p.push(0); // confirmed platform locked content
        p.push(1); // multi player game
        p.push(1); // LAN broadcast
        put_var_i32(&mut p, 6); // XBL broadcast mode
        put_var_i32(&mut p, 6); // platform broadcast mode
        p.push(1); // commands enabled
        p.push(0); // texture pack required
                   // Three game rules, one of each value type.
        put_var_u32(&mut p, 3);
        put_string(&mut p, "dodaylightcycle");
        p.push(0);
        put_var_u32(&mut p, 1);
        p.push(1);
        put_string(&mut p, "randomtickspeed");
        p.push(0);
        put_var_u32(&mut p, 2);
        put_var_u32(&mut p, 3);
        put_string(&mut p, "raindelay");
        p.push(0);
        put_var_u32(&mut p, 3);
        put_f32(&mut p, 1.5);
        // Experiments: a u32-LE count, not a varint.
        p.extend_from_slice(&1u32.to_le_bytes());
        put_string(&mut p, "data_driven_items");
        p.push(1);
        p.push(0); // experiments previously toggled
        p.push(0); // bonus chest
        p.push(0); // start with map
        put_var_i32(&mut p, 1); // player permissions
        p.extend_from_slice(&4i32.to_le_bytes()); // chunk tick radius
                                                  // Locked packs … emote chat muted.
        p.extend(std::iter::repeat_n(0u8, 10));
        put_string(&mut p, "1.21.0"); // base game version
        p.extend_from_slice(&16i32.to_le_bytes()); // limited world width
        p.extend_from_slice(&16i32.to_le_bytes()); // limited world depth
        p.push(1); // new nether
        put_string(&mut p, ""); // education resource button name
        put_string(&mut p, ""); // education resource link
        if experimental_flag_is_optional(protocol) {
            p.push(1); // optional present
            p.push(0); // force experimental gameplay
        } else {
            p.push(0); // force experimental gameplay
        }
        p.push(0); // chat restriction level
        p.push(0); // disable player interactions
        if protocol >= version::TRANSACTION_V2 {
            put_var_i32(&mut p, 0); // server editor connection policy
            p.push(0); // allow anonymous block drops
        }
        if protocol < version::TEXT_NO_CONSTANTS {
            put_string(&mut p, "server-id");
            put_string(&mut p, "world-id");
            put_string(&mut p, "scenario-id");
            if protocol >= version::START_GAME_NO_MOVEMENT_TYPE {
                put_string(&mut p, "owner-id");
            }
        }
        put_string(&mut p, "level-id");
        put_string(&mut p, "world name");
        put_string(&mut p, ""); // template content identity
        p.push(0); // trial
        if protocol < version::START_GAME_NO_MOVEMENT_TYPE {
            put_var_i32(&mut p, 2); // movement type (server authoritative)
        }
        put_var_i32(&mut p, 40); // rewind history size
        p.push(1); // server authoritative block breaking
        p.extend_from_slice(&123i64.to_le_bytes()); // world time
        put_var_i32(&mut p, 99); // enchantment seed
                                 // One custom block, so the NBT skip is exercised.
        put_var_u32(&mut p, 1);
        put_string(&mut p, "custom:block");
        p.extend_from_slice(&[10, 0, 0]); // empty network NBT compound
        if protocol < version::ITEM_REGISTRY_PACKET {
            put_var_u32(&mut p, items.len() as u32);
            for (name, id) in items {
                put_string(&mut p, name);
                p.extend_from_slice(&id.to_le_bytes());
                p.push(0); // component based
            }
        }
        put_string(&mut p, "correlation"); // multiplayer correlation id
        p.push(1); // server authoritative inventory
        put_string(&mut p, "1.21.0"); // game version
        p.extend_from_slice(&[10, 0, 0]); // property data
        p.extend_from_slice(&0u64.to_le_bytes()); // block state checksum
        p.extend_from_slice(&[0u8; 16]); // world template id
        p.push(0); // client side generation
        p.push(hashed as u8); // block network ids are hashes
        p.push(0); // server authoritative sound
        p
    }

    #[test]
    fn start_game_policy_is_read_on_every_protocol() {
        for protocol in [
            766, 776, 786, 800, 818, 827, 844, 859, 860, 898, 924, 944, 975, 1001,
        ] {
            let items = [("minecraft:apple", 257i16), ("minecraft:dirt", 3)];
            let payload = start_game_payload(protocol, &items, true);
            let info = StartGameInfo::decode(&payload, protocol).expect("decode");
            assert_eq!(info.unique_id, 42);
            assert_eq!(info.runtime_id, 43);
            assert_eq!(info.game_mode, 1);
            assert_eq!(info.position, [8.5, 66.62, -4.5]);
            let policy = info
                .policy
                .unwrap_or_else(|| panic!("policy parsed on {protocol}"));
            assert!(
                policy.block_network_ids_are_hashes,
                "hashed ids on {protocol}"
            );
            assert!(policy.server_authoritative_block_breaking, "{protocol}");
            assert!(policy.server_authoritative_inventory, "{protocol}");
            assert_eq!(policy.custom_block_count, 1, "{protocol}");
            // The item table only travels in StartGame before 1.21.60.
            if protocol < version::ITEM_REGISTRY_PACKET {
                assert_eq!(policy.items, items.to_vec(), "{protocol}");
            } else {
                assert!(policy.items.is_empty(), "{protocol}");
            }
        }
    }

    /// The flag is read from the correct field: flipping it in the payload
    /// flips it in the parse, on both sides of every layout change.
    #[test]
    fn start_game_reports_palette_runtime_ids() {
        for protocol in [766, 818, 844, 898, 924, 1001] {
            let payload = start_game_payload(protocol, &[], false);
            let policy = StartGameInfo::decode(&payload, protocol)
                .unwrap()
                .policy
                .unwrap_or_else(|| panic!("policy parsed on {protocol}"));
            assert!(
                !policy.block_network_ids_are_hashes,
                "palette ids on {protocol}"
            );
        }
    }

    /// A truncated or unknown layout must not fail the whole packet: the
    /// leading fields are still needed, and the bot falls back to detecting
    /// the runtime-id mode from chunk data.
    #[test]
    fn start_game_tail_failure_leaves_policy_none() {
        let payload = start_game_payload(898, &[], true);
        let info = StartGameInfo::decode(&payload[..payload.len() - 40], 898).unwrap();
        assert_eq!(info.runtime_id, 43);
        assert!(info.policy.is_none());
    }

    /// Reading a payload with the layout of a different version desynchronises
    /// the walk, which must be reported as "no policy" rather than as a
    /// confidently wrong answer.
    #[test]
    fn start_game_wrong_layout_is_not_trusted() {
        // 766 has a movement type and an item table that 898 does not.
        let payload = start_game_payload(766, &[("minecraft:apple", 257)], true);
        let info = StartGameInfo::decode(&payload, 898).unwrap();
        assert_eq!(
            info.runtime_id, 43,
            "leading fields are version independent"
        );
        let wrong = info
            .policy
            .map(|p| p.block_network_ids_are_hashes)
            .unwrap_or(false);
        assert!(!wrong, "a desynchronised walk must not claim hashed ids");
    }
}
