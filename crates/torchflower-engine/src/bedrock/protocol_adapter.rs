use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use p384::{
    pkcs8::{DecodePrivateKey, DecodePublicKey},
    PublicKey, SecretKey,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt,
    sync::atomic::{AtomicUsize, Ordering},
};
use torchflower_protocol::{
    ClientToServerHandshakePacket, Packet as BedrockProto, ProtocolVersion as ProtoVersion,
    RequestNetworkSettingsPacket,
};

use crate::{
    auth::{minecraft::BedrockJwtChain, minecraft::MinecraftAuth, ProvisionedBedrockSession},
    bedrock::local_network::{codec, compression::Compression, encryption::Encryption},
    bedrock::transport::RaknetClientAdapter,
    error::{EngineError, EngineResult},
};

pub struct BedrockProtocolAdapter {
    transport: RaknetClientAdapter,
    compression: Option<Compression>,
    encryption: Option<Encryption>,
    server_address: String,
    protocol_options: BedrockProtocolOptions,
    version: ProtoVersion,
}

#[derive(Debug, Clone)]
pub struct InboundBatch {
    pub typed: Vec<BedrockProto>,
    pub observed: Vec<ObservedPacket>,
    pub decode_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ObservedPacket {
    PlayStatus(Option<i32>),
    ResourcePacksInfo,
    ResourcePackStack,
    StartGame(Option<ObservedStartGame>),
    Disconnect(Option<ObservedDisconnect>),
    NetworkStackLatency {
        timestamp: u64,
        needs_response: bool,
    },
    Text(Option<ObservedText>),
    ModalFormRequest,
    AddItemEntity(ObservedItemEntity),
    TakeItemEntity {
        runtime_entity_id: u64,
        target_runtime_entity_id: u32,
    },
    InventoryContent {
        items: Vec<ObservedInventoryItem>,
    },
    InventorySlot {
        item: Option<ObservedInventoryItem>,
    },
    ItemStackResponse {
        responses: Vec<ObservedItemStackResponse>,
    },
    LevelChunk {
        chunk_x: i32,
        chunk_z: i32,
        dimension: i32,
        samples: Vec<ObservedBlockSample>,
    },
    NetworkChunkPublisherUpdate {
        x: i32,
        y: i32,
        z: i32,
        radius: u32,
    },
    UpdateBlock {
        x: i32,
        y: i32,
        z: i32,
        runtime_id: u32,
        flags: u32,
        layer: u32,
    },
    ContainerOpen {
        container_id: u8,
        container_type: u8,
    },
    UpdateSoftEnum,
    RegistryKnown(u32),
    Other(u32),
}

#[derive(Debug, Clone)]
pub struct ObservedStartGame {
    pub entity_id: i64,
    pub runtime_id: u64,
    pub position: (f32, f32, f32),
    pub rotation: (f32, f32),
    pub disable_player_interactions: Option<bool>,
    pub server_authoritative_block_breaking: Option<bool>,
    pub block_network_ids_are_hashes: Option<bool>,
    pub block_property_count: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ObservedDisconnect {
    pub reason: i32,
    pub hide_reason: bool,
    pub message: Option<String>,
    pub filtered_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ObservedInventoryItem {
    pub container_id: u32,
    pub slot: u32,
    pub item_id: i32,
    pub stack_id: Option<i32>,
    pub container_type: Option<u8>,
    pub dynamic_container_id: Option<u32>,
    pub item_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ObservedItemStackResponse {
    pub result: u8,
    pub result_name: &'static str,
    pub raw_client_request_id: u32,
    pub client_request_id: i32,
    pub trailing_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct ObservedText {
    pub strings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ObservedBlockSample {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub runtime_id: u32,
}

#[derive(Debug, Clone)]
pub struct ObservedItemEntity {
    pub entity_id: i64,
    pub runtime_entity_id: u64,
    pub item_id: i32,
    pub stack_id: Option<i32>,
    pub position: (f32, f32, f32),
    pub velocity: (f32, f32, f32),
    pub item_bytes: Vec<u8>,
}

pub const DEFAULT_BEDROCK_PROTOCOL_VERSION: i32 = 898;
pub const TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV: &str = "TORCHFLOWER_BEDROCK_PROTOCOL_VERSION";
pub const LEGACY_BEDROCK_PROTOCOL_VERSION_ENV: &str = "BEDROCK_PROTOCOL_VERSION";

const NETWORK_STACK_LATENCY_STRATEGY: &str = "scaled_microseconds";
const NETWORK_STACK_LATENCY_MAGNITUDE: u64 = 1_000_000;
const NETWORK_STACK_LATENCY_DEFAULT_LOG_LIMIT: usize = 8;
static NETWORK_STACK_LATENCY_RX_LOGS: AtomicUsize = AtomicUsize::new(0);
static NETWORK_STACK_LATENCY_TX_LOGS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedrockProtocolVersionSource {
    Config,
    TorchflowerEnv,
    LegacyEnv,
    Default,
}

impl fmt::Display for BedrockProtocolVersionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => f.write_str("config"),
            Self::TorchflowerEnv => f.write_str(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV),
            Self::LegacyEnv => f.write_str(LEGACY_BEDROCK_PROTOCOL_VERSION_ENV),
            Self::Default => f.write_str("default"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BedrockProtocolOptions {
    pub requested_protocol_version: i32,
    pub codec_protocol_version: ProtoVersion,
    pub source: BedrockProtocolVersionSource,
}

impl BedrockProtocolOptions {
    pub fn from_config(protocol_version: i32) -> EngineResult<Self> {
        Self::from_source(protocol_version, BedrockProtocolVersionSource::Config)
    }

    pub fn from_env_or_default() -> EngineResult<Self> {
        Self::resolve(None)
    }

    pub fn resolve(config_protocol_version: Option<i32>) -> EngineResult<Self> {
        if let Some(protocol_version) = config_protocol_version {
            return Self::from_source(protocol_version, BedrockProtocolVersionSource::Config);
        }

        if let Ok(value) = env::var(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV) {
            return Self::from_source(
                parse_protocol_version_env(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV, &value)?,
                BedrockProtocolVersionSource::TorchflowerEnv,
            );
        }

        if let Ok(value) = env::var(LEGACY_BEDROCK_PROTOCOL_VERSION_ENV) {
            return Self::from_source(
                parse_protocol_version_env(LEGACY_BEDROCK_PROTOCOL_VERSION_ENV, &value)?,
                BedrockProtocolVersionSource::LegacyEnv,
            );
        }

        Self::from_source(
            DEFAULT_BEDROCK_PROTOCOL_VERSION,
            BedrockProtocolVersionSource::Default,
        )
    }

    pub fn codec_protocol_version_number(&self) -> i32 {
        self.codec_protocol_version.to_u32() as i32
    }

    pub fn codec_exact_match(&self) -> bool {
        self.requested_protocol_version == self.codec_protocol_version_number()
    }

    fn from_source(
        requested_protocol_version: i32,
        source: BedrockProtocolVersionSource,
    ) -> EngineResult<Self> {
        validate_protocol_version(source, requested_protocol_version)?;
        Ok(Self {
            requested_protocol_version,
            codec_protocol_version: codec_protocol_version_for(requested_protocol_version),
            source,
        })
    }
}

fn parse_protocol_version_env(name: &str, value: &str) -> EngineResult<i32> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(EngineError::Bedrock(format!(
            "{name} must be a positive integer, got an empty value"
        )));
    }

    trimmed.parse::<i32>().map_err(|err| {
        EngineError::Bedrock(format!(
            "{name} must be a positive integer, got {trimmed:?}: {err}"
        ))
    })
}

fn validate_protocol_version(
    source: BedrockProtocolVersionSource,
    protocol_version: i32,
) -> EngineResult<()> {
    if protocol_version <= 0 {
        return Err(EngineError::Bedrock(format!(
            "Bedrock protocol version from {source} must be a positive integer, got {protocol_version}"
        )));
    }
    Ok(())
}

fn codec_protocol_version_for(protocol_version: i32) -> ProtoVersion {
    match protocol_version {
        662 => ProtoVersion::V662,
        766 => ProtoVersion::V766,
        898 => ProtoVersion::V898,
        975 => ProtoVersion::V975,
        _ => ProtoVersion::V898,
    }
}

fn env_flag(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            let value = value.trim();
            !(value == "0" || value.eq_ignore_ascii_case("false"))
        })
        .unwrap_or(default)
}

fn trace_packets_enabled() -> bool {
    env_flag("BEDROCK_TRACE_PACKETS", false)
}

fn trace_chunks_enabled() -> bool {
    env_flag("BEDROCK_TRACE_CHUNKS", false)
}

fn should_log_limited(counter: &AtomicUsize, limit: usize) -> bool {
    trace_packets_enabled() || counter.fetch_add(1, Ordering::Relaxed) < limit
}

impl BedrockProtocolAdapter {
    pub async fn connect(host: &str, port: u16) -> EngineResult<Self> {
        Self::connect_with_options(host, port, BedrockProtocolOptions::from_env_or_default()?).await
    }

    pub async fn connect_with_options(
        host: &str,
        port: u16,
        protocol_options: BedrockProtocolOptions,
    ) -> EngineResult<Self> {
        let version = protocol_options.codec_protocol_version;
        Ok(Self {
            transport: RaknetClientAdapter::connect(host, port, 11).await?,
            compression: None,
            encryption: None,
            server_address: login_server_address(host, port),
            protocol_options,
            version,
        })
    }

    pub async fn request_network_settings(&mut self) -> EngineResult<()> {
        let protocol_ver = self.protocol_options.requested_protocol_version;
        let codec_protocol_ver = self.protocol_options.codec_protocol_version_number();
        let parts: Vec<&str> = self.server_address.split(':').collect();
        let host = parts.first().copied().unwrap_or("");
        let port = parts.get(1).copied().unwrap_or("");

        let packet = BedrockProto::RequestNetworkSettings(RequestNetworkSettingsPacket {
            protocol_version: protocol_ver,
        });
        let version = self.version;
        let payload = codec::encode_packets(&[packet], None, None, version)
            .map_err(|err| EngineError::Bedrock(format!("encode RequestNetworkSettings: {err}")))?;

        tracing::warn!(
            "[NETWORK_SETTINGS_TX] protocol_version={} codec_protocol_version={} host={} port={} payload_len={}",
            protocol_ver,
            codec_protocol_ver,
            host,
            port,
            payload.len()
        );
        if !self.protocol_options.codec_exact_match() {
            tracing::warn!(
                "[BEDROCK_PROTOCOL] requested_protocol_version={} codec_protocol_version={} codec_exact_match=false source={}",
                protocol_ver,
                codec_protocol_ver,
                self.protocol_options.source
            );
        }
        tracing::debug!("[NETWORK_SETTINGS_TX] bytes={}", hex_dump(&payload));

        self.transport.send_game_packet(&payload).await?;
        let response = self.transport.recv_game_packet().await?;
        let packets = codec::decode_packets(response, None, None, version)
            .map_err(|err| EngineError::Bedrock(format!("decode NetworkSettings: {err}")))?;
        tracing::debug!("[NETWORK_SETTINGS_RX] packets={:?}", packets);

        let mut early_disconnect = None;
        for packet in &packets {
            if let BedrockProto::Disconnect(ref p) = packet {
                early_disconnect = Some(p.clone());
                tracing::warn!(
                    "[NETWORK_SETTINGS_RX] early_disconnect=true reason={} hide_reason={} message={:?}",
                    p.reason, p.hide_reason, p.message
                );
            }
        }

        for packet in packets {
            if let BedrockProto::NetworkSettings(settings) = packet {
                tracing::warn!(
                    "[NETWORK_SETTINGS_RX] success=true compression_algorithm={} compression_threshold={} protocol_version={} codec_protocol_version={}",
                    settings.compression_algorithm,
                    settings.compression_threshold,
                    protocol_ver,
                    codec_protocol_ver
                );
                self.compression = Some(match settings.compression_algorithm {
                    0 => Compression::Zlib {
                        threshold: settings.compression_threshold,
                        compression_level: 7,
                    },
                    1 => Compression::Snappy {
                        threshold: settings.compression_threshold,
                    },
                    _ => Compression::None,
                });
                return Ok(());
            }
        }

        if let Some(p) = early_disconnect {
            return Err(EngineError::Bedrock(format!(
                "server did not return NetworkSettingsPacket; received early Disconnect before NetworkSettings. \
                 protocol_version={}. codec_protocol_version={}. reason={}. hide_reason={}. message={:?}. \
                 This usually means unsupported protocol version, invalid RequestNetworkSettings encoding/framing, or server rejected the client before login.",
                protocol_ver, codec_protocol_ver, p.reason, p.hide_reason, p.message
            )));
        }

        Err(EngineError::Bedrock(format!(
            "server did not return NetworkSettingsPacket; protocol_version={}. codec_protocol_version={}. \
             This usually means unsupported protocol version, invalid RequestNetworkSettings encoding/framing, or server rejected the client before login.",
            protocol_ver, codec_protocol_ver
        )))
    }

    pub fn protocol_options(&self) -> BedrockProtocolOptions {
        self.protocol_options
    }

    pub async fn send_login(&mut self, session: &ProvisionedBedrockSession) -> EngineResult<()> {
        let login_token = login_token_for_session(session);
        let connection_request = match override_connection_request()? {
            Some(connection_request) => connection_request,
            None => MinecraftAuth::connection_request(
                &session.chain,
                login_token,
                &self.server_address,
                Some(session.playfab_id.as_str()),
            )?,
        };
        tracing::info!(
            "[LOGIN] sending login: chain_count={} connection_request_len={} skin_len={}",
            session.chain.chain.len(),
            connection_request.len(),
            session.chain.skin.len()
        );
        tracing::debug!(
            "[LOGIN] fingerprint: {:?}",
            MinecraftAuth::connection_request_fingerprint(&connection_request)?
        );
        tracing::debug!(
            "[LOGIN] chain_fingerprint: {}",
            MinecraftAuth::bedrock_chain_fingerprint(&session.chain, login_token)?
        );
        let payload = Self::encode_login_packet_batch_with_protocol(
            &connection_request,
            self.compression.as_ref(),
            self.protocol_options.requested_protocol_version,
        )?;
        self.transport.send_game_packet(&payload).await
    }

    // Bedrock Login wraps LoginTokens in an `encapsulated` field: a varint byte
    // length followed by the two LittleString JWT fields. Keep the layout
    // explicit here so TorchFlower can byte-compare it against bedrock-protocol.
    pub fn encode_login_packet_batch(
        connection_request: &[u8],
        compression: Option<&Compression>,
    ) -> EngineResult<Vec<u8>> {
        let protocol_options = BedrockProtocolOptions::from_env_or_default()?;
        Self::encode_login_packet_batch_with_protocol(
            connection_request,
            compression,
            protocol_options.requested_protocol_version,
        )
    }

    pub fn encode_login_packet_batch_with_protocol(
        connection_request: &[u8],
        compression: Option<&Compression>,
        protocol_version: i32,
    ) -> EngineResult<Vec<u8>> {
        validate_protocol_version(BedrockProtocolVersionSource::Config, protocol_version)?;

        let mut packet = Vec::with_capacity(8 + connection_request.len());
        write_unsigned_varint_u32(1, &mut packet);
        packet.extend_from_slice(&protocol_version.to_be_bytes());
        write_unsigned_varint_u32(connection_request.len() as u32, &mut packet);
        packet.extend_from_slice(connection_request);

        let mut batch = Vec::with_capacity(5 + packet.len());
        write_unsigned_varint_u32(packet.len() as u32, &mut batch);
        batch.extend_from_slice(&packet);

        codec::compress_packets(batch, compression)
            .map_err(|err| EngineError::Bedrock(format!("encode Login packet batch: {err}")))
    }

    fn play_status_to_string(status: i32) -> &'static str {
        match status {
            0 => "LoginSuccess",
            1 => "FailedClient (outdated client)",
            2 => "FailedServer (outdated server)",
            3 => "PlayerSpawn",
            4 => "InvalidTenant",
            5 => "EditionMismatchEduToVanilla",
            6 => "EditionMismatchVanillaToEdu",
            7 => "FailedMaxPlayers",
            8 => "FailedServerFull",
            _ => "Unknown",
        }
    }

    fn log_handshake_packet(packet: &BedrockProto) {
        match packet {
            BedrockProto::ServerToClientHandshake(ref handshake) => {
                tracing::info!(
                    "[LOGIN_HANDSHAKE] rx ServerToClientHandshake (ID={:#04x}): token_len={}",
                    packet.id(),
                    handshake.handshake_web_token.len()
                );
            }
            BedrockProto::PlayStatus(ref p) => {
                tracing::info!(
                    "[LOGIN_HANDSHAKE] rx PlayStatus (ID={:#04x}): status={} ({})",
                    packet.id(),
                    p.status,
                    Self::play_status_to_string(p.status)
                );
            }
            BedrockProto::Disconnect(ref disc) => {
                tracing::warn!(
                    "[LOGIN_HANDSHAKE] rx Disconnect (ID={:#04x}): reason={}, hide_reason={}, message={:?}",
                    packet.id(),
                    disc.reason,
                    disc.hide_reason,
                    disc.message
                );
            }
            BedrockProto::ResourcePacksInfo(ref info) => {
                tracing::info!(
                    "[LOGIN_HANDSHAKE] rx ResourcePacksInfo (ID={:#04x}): must_accept={}, has_addons={}, behavior_packs_len={}, resource_packs_len={}",
                    packet.id(),
                    info.must_accept,
                    info.has_addons,
                    info.behavior_packs.len(),
                    info.resource_packs.len()
                );
            }
            BedrockProto::ResourcePackStack(ref stack) => {
                tracing::info!(
                    "[LOGIN_HANDSHAKE] rx ResourcePackStack (ID={:#04x}): must_accept={}, behavior_packs_len={}, resource_packs_len={}, game_version={}",
                    packet.id(),
                    stack.must_accept,
                    stack.behavior_packs.len(),
                    stack.resource_packs.len(),
                    &stack.game_version
                );
            }
            other => {
                tracing::info!(
                    "[LOGIN_HANDSHAKE] rx other packet (ID={:#04x}): {:?}",
                    other.id(),
                    other
                );
            }
        }
    }

    pub async fn complete_login_handshake(
        &mut self,
        chain: &BedrockJwtChain,
    ) -> EngineResult<(Vec<BedrockProto>, bool)> {
        let mut pending = Vec::new();
        let mut encryption_enabled = false;
        loop {
            let recv_res = self.recv().await;
            let packets = match recv_res {
                Ok(p) => p,
                Err(err) => {
                    return Err(EngineError::Bedrock(format!(
                        "login handshake packet decompression/framing error: {err}"
                    )));
                }
            };
            for packet in packets {
                Self::log_handshake_packet(&packet);
                match packet {
                    BedrockProto::ServerToClientHandshake(handshake) => {
                        self.encryption = Some(Self::derive_encryption_from_handshake(
                            chain,
                            &handshake.handshake_web_token,
                        )?);
                        self.send(&[BedrockProto::ClientToServerHandshake(
                            ClientToServerHandshakePacket {},
                        )])
                        .await?;
                        encryption_enabled = true;
                    }
                    BedrockProto::PlayStatus(status_packet) => {
                        if status_packet.status != 0 && status_packet.status != 3 {
                            return Err(EngineError::Bedrock(format!(
                                "Server returned PlayStatus failure: {} ({})",
                                status_packet.status,
                                Self::play_status_to_string(status_packet.status)
                            )));
                        }
                        pending.push(BedrockProto::PlayStatus(status_packet));
                    }
                    BedrockProto::Disconnect(disc) => {
                        if chain.xuid == "0" {
                            return Err(EngineError::Bedrock(format!(
                                "Server rejected offline/mock login (xuid=0). Real Xbox Live authentication is required. Configure an authenticated account using account_id/auth settings. Offline/mock username-only login is only for local/offline testing. Disconnect reason={}, message={:?}",
                                disc.reason,
                                disc.message
                            )));
                        } else {
                            return Err(EngineError::Bedrock(format!(
                                "Server disconnected during login handshake. Reason: {}, HideReason: {}, Message: {:?}",
                                disc.reason,
                                disc.hide_reason,
                                disc.message
                            )));
                        }
                    }
                    other => {
                        pending.push(other);
                    }
                }
            }
            if encryption_enabled
                || pending.iter().any(|p| {
                    matches!(
                        p,
                        BedrockProto::PlayStatus(_)
                            | BedrockProto::ResourcePacksInfo(_)
                            | BedrockProto::ResourcePackStack(_)
                            | BedrockProto::StartGame(_)
                    )
                })
            {
                break;
            }
        }
        Ok((pending, encryption_enabled))
    }

    pub fn derive_encryption_from_handshake(
        chain: &BedrockJwtChain,
        handshake_web_token: &str,
    ) -> EngineResult<Encryption> {
        let header = decode_header(handshake_web_token)
            .map_err(|err| EngineError::Crypto(format!("decode handshake JWT header: {err}")))?;
        let public_der_base64 = header
            .x5u
            .ok_or_else(|| EngineError::Crypto("handshake JWT missing x5u".to_string()))?;
        let public_der = decode_base64(&public_der_base64)?;
        let public_key = PublicKey::from_public_key_der(&public_der)
            .map_err(|err| EngineError::Crypto(format!("decode handshake public key: {err}")))?;

        let mut validation = Validation::new(Algorithm::ES384);
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        validation.validate_aud = false;
        let claims = decode::<ServerHandshakeClaims>(
            handshake_web_token,
            &DecodingKey::from_ec_der(&public_key.to_sec1_bytes()),
            &validation,
        )
        .map_err(|err| EngineError::Crypto(format!("verify handshake JWT: {err}")))?
        .claims;

        let salt = decode_base64(&claims.salt)?;
        let salt: [u8; 16] = salt.try_into().map_err(|salt: Vec<u8>| {
            EngineError::Crypto(format!(
                "handshake salt must be 16 bytes, got {}",
                salt.len()
            ))
        })?;
        let private_key = SecretKey::from_pkcs8_pem(&chain.private_key_pem)
            .map_err(|err| EngineError::Crypto(format!("decode client private key: {err}")))?;
        Ok(Encryption::new(&private_key, &public_key, &salt))
    }

    pub async fn send(&mut self, packets: &[BedrockProto]) -> EngineResult<()> {
        if self.encryption.is_some() && trace_packets_enabled() {
            let plain_batch = codec::encode_packets(packets, None, None, self.version)
                .map_err(|err| EngineError::Bedrock(format!("trace encode packet batch: {err}")))?;
            log_packet_summaries("TX", &plain_batch);
        }

        let payload = codec::encode_packets(
            packets,
            self.compression.as_ref(),
            self.encryption.as_mut(),
            self.version,
        )
        .map_err(|err| EngineError::Bedrock(format!("encode packet batch: {err}")))?;
        self.transport.send_game_packet(&payload).await
    }

    pub async fn send_network_stack_latency_response(
        &mut self,
        timestamp: u64,
    ) -> EngineResult<()> {
        let response_timestamp = network_stack_latency_response_timestamp(timestamp);
        let response_payload = encode_network_stack_latency_payload(response_timestamp, false);
        let plain_batch = encode_network_stack_latency_packet_stream(response_timestamp, false);
        if should_log_limited(
            &NETWORK_STACK_LATENCY_TX_LOGS,
            NETWORK_STACK_LATENCY_DEFAULT_LOG_LIMIT,
        ) {
            eprintln!(
                "[BEDROCK_TX] id=115 name=network_stack_latency incoming_timestamp={} response_timestamp={} needs_response=false strategy={} payload_hex={}",
                timestamp,
                response_timestamp,
                NETWORK_STACK_LATENCY_STRATEGY,
                hex_dump(&response_payload)
            );
        }
        if trace_packets_enabled() {
            log_packet_summaries("TX", &plain_batch);
        }

        let compressed = codec::compress_packets(plain_batch, self.compression.as_ref())
            .map_err(|err| EngineError::Bedrock(format!("compress NetworkStackLatency: {err}")))?;
        let encrypted = codec::encrypt_packets(compressed, self.encryption.as_mut())
            .map_err(|err| EngineError::Bedrock(format!("encrypt NetworkStackLatency: {err}")))?;
        self.transport.send_game_packet(&encrypted).await
    }

    pub async fn send_preencoded_packet_stream(
        &mut self,
        label: &str,
        plain_batch: Vec<u8>,
    ) -> EngineResult<()> {
        if trace_packets_enabled() {
            eprintln!(
                "[BEDROCK_TX_RAW] label={} packet_stream_len={} payload_hex={}",
                label,
                plain_batch.len(),
                hex_dump(&plain_batch)
            );
            log_packet_summaries("TX", &plain_batch);
        }

        let compressed = codec::compress_packets(plain_batch, self.compression.as_ref())
            .map_err(|err| EngineError::Bedrock(format!("compress {label}: {err}")))?;
        let encrypted = codec::encrypt_packets(compressed, self.encryption.as_mut())
            .map_err(|err| EngineError::Bedrock(format!("encrypt {label}: {err}")))?;
        self.transport.send_game_packet(&encrypted).await
    }

    pub async fn send_preencoded_packet_stream_queued(
        &mut self,
        label: &str,
        plain_batch: Vec<u8>,
    ) -> EngineResult<()> {
        if trace_packets_enabled() {
            eprintln!(
                "[BEDROCK_TX_RAW] label={} packet_stream_len={} queued=true payload_hex={}",
                label,
                plain_batch.len(),
                hex_dump(&plain_batch)
            );
            log_packet_summaries("TX", &plain_batch);
        }

        let compressed = codec::compress_packets(plain_batch, self.compression.as_ref())
            .map_err(|err| EngineError::Bedrock(format!("compress {label}: {err}")))?;
        let encrypted = codec::encrypt_packets(compressed, self.encryption.as_mut())
            .map_err(|err| EngineError::Bedrock(format!("encrypt {label}: {err}")))?;
        self.transport.send_game_packet_queued(&encrypted).await
    }

    pub async fn recv(&mut self) -> EngineResult<Vec<BedrockProto>> {
        let payload = self.transport.recv_game_packet().await?;
        if trace_packets_enabled() {
            eprintln!(
                "[RECV] got game payload len={} first_bytes={:02x?}",
                payload.len(),
                &payload[..payload.len().min(16)]
            );
        }
        let packet_stream = self.prepare_inbound_packet_stream(payload)?;
        let (packet_stream, _) = filter_locally_decoded_packets(&packet_stream)?;
        match codec::decode_packets(packet_stream, None, None, self.version) {
            Ok(packets) => Ok(packets),
            Err(err) => {
                eprintln!("[BEDROCK_DECODE_ERROR] decode packet batch: {err}");
                Err(EngineError::Bedrock(format!("decode packet batch: {err}")))
            }
        }
    }

    pub async fn recv_lossy(&mut self) -> EngineResult<InboundBatch> {
        let payload = self.transport.recv_game_packet().await?;
        if trace_packets_enabled() {
            eprintln!(
                "[RECV] got game payload len={} first_bytes={:02x?}",
                payload.len(),
                &payload[..payload.len().min(16)]
            );
        }

        let packet_stream = self.prepare_inbound_packet_stream(payload)?;
        let observed = observe_packet_ids(&packet_stream)?;
        let (filtered_packet_stream, locally_observed) =
            filter_locally_decoded_packets(&packet_stream)?;
        match codec::decode_packets(filtered_packet_stream, None, None, self.version) {
            Ok(typed) => Ok(InboundBatch {
                typed,
                observed: merge_locally_observed_with_raw_keepalives(observed, locally_observed),
                decode_error: None,
            }),
            Err(err) => {
                let error = err.to_string();
                eprintln!("[BEDROCK_DECODE_ERROR] decode packet batch: {error}");
                eprintln!(
                    "[RECV] observed packet summary after decode error: {}",
                    observed_packet_summary(&observed)
                );
                Ok(InboundBatch {
                    typed: Vec::new(),
                    observed,
                    decode_error: Some(error),
                })
            }
        }
    }

    pub async fn recv_lenient(&mut self) -> EngineResult<InboundBatch> {
        let payload = self.transport.recv_game_packet().await?;
        if trace_packets_enabled() {
            eprintln!(
                "[RECV] got game payload len={} first_bytes={:02x?}",
                payload.len(),
                &payload[..payload.len().min(16)]
            );
        }

        let packet_stream = self.prepare_inbound_packet_stream(payload)?;
        let observed = observe_packet_ids(&packet_stream)?;
        if trace_packets_enabled() {
            eprintln!(
                "[RECV] observed packet summary: {}",
                observed_packet_summary(&observed)
            );
        }

        Ok(InboundBatch {
            typed: Vec::new(),
            observed,
            decode_error: None,
        })
    }

    /// Receives the next game batch and returns the decrypted, decompressed
    /// packet stream (length-prefixed packets) without decoding it. Used by
    /// `torchflower-bot`, which performs its own zero-copy decoding.
    pub async fn recv_raw(&mut self) -> EngineResult<Vec<u8>> {
        let payload = self.transport.recv_game_packet().await?;
        self.prepare_inbound_packet_stream(payload)
    }

    pub async fn close(&self) {
        self.transport.close().await;
    }

    fn prepare_inbound_packet_stream(&mut self, payload: Vec<u8>) -> EngineResult<Vec<u8>> {
        let encrypted = self.encryption.is_some();
        let compressed = self.compression.is_some();
        let mut packet_stream = codec::decrypt_packets(payload, self.encryption.as_mut())
            .map_err(|err| EngineError::Bedrock(format!("decrypt packet batch: {err}")))?;
        if encrypted && trace_packets_enabled() {
            eprintln!(
                "[BEDROCK_CRYPTO] decrypted_batch_len={}",
                packet_stream.len()
            );
            eprintln!(
                "[BEDROCK_PIPELINE] after_decrypt decrypted_size={}",
                packet_stream.len()
            );
        }

        packet_stream = codec::decompress_packets(packet_stream, self.compression.as_ref())
            .map_err(|err| EngineError::Bedrock(format!("decompress packet batch: {err}")))?;
        if compressed && trace_packets_enabled() {
            eprintln!(
                "[BEDROCK_COMPRESSION] algorithm={} decompressed_batch_len={}",
                compression_name(self.compression.as_ref()),
                packet_stream.len()
            );
            eprintln!(
                "[BEDROCK_PIPELINE] after_decompress decompressed_size={}",
                packet_stream.len()
            );
        }

        if encrypted && trace_packets_enabled() {
            match count_packet_summaries(&packet_stream) {
                Ok(packet_count) => {
                    eprintln!("[BEDROCK_PIPELINE] before_decode packet_count={packet_count}");
                }
                Err(err) => {
                    eprintln!("[BEDROCK_PIPELINE] before_decode packet_count_error={err}");
                }
            }
            log_packet_summaries("RX", &packet_stream);
        }

        Ok(packet_stream)
    }
}

#[derive(Debug, Deserialize)]
struct ServerHandshakeClaims {
    salt: String,
}

fn decode_base64(value: &str) -> EngineResult<Vec<u8>> {
    STANDARD
        .decode(value)
        .or_else(|_| URL_SAFE_NO_PAD.decode(value))
        .map_err(|err| EngineError::Crypto(format!("decode base64 handshake material: {err}")))
}

fn write_unsigned_varint_u32(mut value: u32, out: &mut Vec<u8>) {
    loop {
        if (value & !0x7f) == 0 {
            out.push(value as u8);
            break;
        }
        out.push(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
}

fn observe_packet_ids(packet_stream: &[u8]) -> EngineResult<Vec<ObservedPacket>> {
    let mut offset = 0usize;
    let mut observed = Vec::new();

    while let Some(summary) = read_packet_summary(packet_stream, &mut offset)? {
        let packet_id = summary.packet_id;
        observed.push(match packet_id {
            0x02 => {
                ObservedPacket::PlayStatus(read_play_status(summary.packet, summary.payload_offset))
            }
            0x05 => ObservedPacket::Disconnect(read_disconnect(
                summary.packet,
                summary.payload_offset,
            )),
            0x06 => ObservedPacket::ResourcePacksInfo,
            0x07 => ObservedPacket::ResourcePackStack,
            0x09 => ObservedPacket::Text(read_text_observation(
                summary.packet,
                summary.payload_offset,
            )),
            0x0b => ObservedPacket::StartGame(read_start_game_observation(
                summary.packet,
                summary.payload_offset,
            )),
            0x0f => read_add_item_entity(summary.packet, summary.payload_offset)
                .unwrap_or(ObservedPacket::Other(packet_id)),
            0x11 => read_take_item_entity(summary.packet, summary.payload_offset)
                .unwrap_or(ObservedPacket::Other(packet_id)),
            0x15 => read_update_block(summary.packet, summary.payload_offset)
                .unwrap_or(ObservedPacket::Other(packet_id)),
            0x3a => read_level_chunk_observation(summary.packet, summary.payload_offset)
                .unwrap_or(ObservedPacket::RegistryKnown(packet_id)),
            0x31 => ObservedPacket::InventoryContent {
                items: match read_inventory_content(summary.packet, summary.payload_offset) {
                    Some(items) => items,
                    None => {
                        eprintln!(
                            "[GAMEPLAY_INVENTORY_RAW] packet=InventoryContent decode_failed=true payload_len={}",
                            summary.payload_len
                        );
                        Vec::new()
                    }
                },
            },
            0x32 => ObservedPacket::InventorySlot {
                item: match read_inventory_slot(summary.packet, summary.payload_offset) {
                    Some(item) => item,
                    None => {
                        eprintln!(
                            "[GAMEPLAY_INVENTORY_RAW] packet=InventorySlot decode_failed=true payload_len={}",
                            summary.payload_len
                        );
                        None
                    }
                },
            },
            0x94 => ObservedPacket::ItemStackResponse {
                responses: match read_item_stack_response(summary.packet, summary.payload_offset) {
                    Some(responses) => responses,
                    None => {
                        eprintln!(
                            "[GAMEPLAY_ITEM_STACK_RESPONSE_RAW] decode_failed=true payload_len={}",
                            summary.payload_len
                        );
                        Vec::new()
                    }
                },
            },
            0x2e => read_container_open_observed(summary.packet, summary.payload_offset)
                .unwrap_or(ObservedPacket::Other(packet_id)),
            0x64 => ObservedPacket::ModalFormRequest,
            0x72 => ObservedPacket::UpdateSoftEnum,
            0x73 => read_network_stack_latency(summary.packet, summary.payload_offset)
                .unwrap_or(ObservedPacket::Other(packet_id)),
            0x79 => read_network_chunk_publisher_update(summary.packet, summary.payload_offset)
                .unwrap_or(ObservedPacket::RegistryKnown(packet_id)),
            _ if bedrock_packet_registry_name(packet_id).is_some() => {
                ObservedPacket::RegistryKnown(packet_id)
            }
            _ => ObservedPacket::Other(packet_id),
        });
    }

    Ok(observed)
}

#[derive(Debug)]
struct PacketSummary<'a> {
    packet: &'a [u8],
    packet_id: u32,
    payload_len: usize,
    payload_offset: usize,
}

fn observed_packet_summary(observed: &[ObservedPacket]) -> String {
    let mut frequencies = BTreeMap::<String, usize>::new();
    for packet in observed {
        *frequencies
            .entry(observed_packet_label(packet).to_string())
            .or_default() += 1;
    }

    if frequencies.is_empty() {
        return "none".to_string();
    }

    frequencies
        .into_iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn observed_packet_label(packet: &ObservedPacket) -> String {
    match packet {
        ObservedPacket::PlayStatus(_) => "play_status".to_string(),
        ObservedPacket::ResourcePacksInfo => "resource_packs_info".to_string(),
        ObservedPacket::ResourcePackStack => "resource_pack_stack".to_string(),
        ObservedPacket::StartGame(_) => "start_game".to_string(),
        ObservedPacket::Disconnect(_) => "disconnect".to_string(),
        ObservedPacket::NetworkStackLatency { .. } => "network_stack_latency".to_string(),
        ObservedPacket::Text(_) => "text".to_string(),
        ObservedPacket::ModalFormRequest => "modal_form_request".to_string(),
        ObservedPacket::AddItemEntity(_) => "add_item_entity".to_string(),
        ObservedPacket::TakeItemEntity { .. } => "take_item_entity".to_string(),
        ObservedPacket::InventoryContent { .. } => "inventory_content".to_string(),
        ObservedPacket::InventorySlot { .. } => "inventory_slot".to_string(),
        ObservedPacket::ItemStackResponse { .. } => "item_stack_response".to_string(),
        ObservedPacket::LevelChunk { .. } => "level_chunk".to_string(),
        ObservedPacket::NetworkChunkPublisherUpdate { .. } => {
            "network_chunk_publisher_update".to_string()
        }
        ObservedPacket::UpdateBlock { .. } => "update_block".to_string(),
        ObservedPacket::ContainerOpen { .. } => "container_open".to_string(),
        ObservedPacket::UpdateSoftEnum => "update_soft_enum".to_string(),
        ObservedPacket::RegistryKnown(packet_id) => bedrock_packet_name(*packet_id).to_string(),
        ObservedPacket::Other(packet_id) => format!("other({packet_id})"),
    }
}

fn log_packet_summaries(direction: &str, packet_stream: &[u8]) {
    let mut offset = 0usize;
    let mut frequencies = BTreeMap::<u32, usize>::new();
    loop {
        match read_packet_summary(packet_stream, &mut offset) {
            Ok(Some(summary)) => {
                *frequencies.entry(summary.packet_id).or_default() += 1;
                let name = bedrock_packet_name(summary.packet_id);
                let known = bedrock_packet_registry_name(summary.packet_id).is_some();
                eprintln!(
                    "[BEDROCK_{direction}] id={} name={} len={}",
                    summary.packet_id, name, summary.payload_len
                );
                eprintln!(
                    "[PACKET_REGISTRY] id={} name={} len={} known={}",
                    summary.packet_id, name, summary.payload_len, known
                );
                if direction == "RX" && summary.packet_id == 0x0b {
                    trace_start_game_packet(summary.packet, summary.payload_offset);
                }
                if direction == "RX" && summary.packet_id == 0x72 {
                    let _ = TorchFlowerUpdateSoftEnumPacket::deserialize_with_trace(
                        summary.packet,
                        summary.payload_offset,
                    );
                }
                if direction == "RX" && summary.packet_id == 0x4c {
                    log_loose_text_candidates(
                        "AVAILABLE_COMMANDS",
                        summary.packet,
                        summary.payload_offset,
                    );
                }
                if direction == "RX" && summary.packet_id == 0x4f {
                    log_loose_text_candidates(
                        "COMMAND_OUTPUT",
                        summary.packet,
                        summary.payload_offset,
                    );
                }
                if direction == "RX" {
                    trace_known_packet_impl(&summary);
                }
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("[BEDROCK_{direction}] log_error={err}");
                break;
            }
        }
    }

    for (packet_id, count) in frequencies {
        eprintln!(
            "[PACKET_FREQ] id={} count={} known={}",
            packet_id,
            count,
            bedrock_packet_registry_name(packet_id).is_some()
        );
    }
}

fn log_loose_text_candidates(label: &str, packet: &[u8], payload_offset: usize) {
    let candidates = loose_text_candidates(packet, payload_offset);
    let payload_len = packet.len().saturating_sub(payload_offset);
    if candidates.is_empty() {
        eprintln!("[{label}] candidates=none payload_len={payload_len}");
        return;
    }

    let prioritized = prioritize_command_text_candidates(&candidates);
    let selected = if prioritized.is_empty() {
        candidates.iter().take(80).cloned().collect::<Vec<_>>()
    } else {
        prioritized
    };
    eprintln!(
        "[{label}] payload_len={} candidate_count={} shown_count={} candidates={}",
        payload_len,
        candidates.len(),
        selected.len(),
        selected
            .iter()
            .map(|candidate| sanitize_log_text(candidate))
            .collect::<Vec<_>>()
            .join(" | ")
    );
}

fn loose_text_candidates(packet: &[u8], payload_offset: usize) -> Vec<String> {
    let mut candidates = BTreeSet::new();
    for start in payload_offset..packet.len() {
        let mut offset = start;
        let Some(len) = read_trace_var_u32(packet, &mut offset).map(|len| len as usize) else {
            continue;
        };
        if !(2..=160).contains(&len) {
            continue;
        }
        let Some(end) = offset.checked_add(len).filter(|end| *end <= packet.len()) else {
            continue;
        };
        let bytes = &packet[offset..end];
        let Ok(value) = std::str::from_utf8(bytes) else {
            continue;
        };
        let value = value.trim();
        if is_loose_packet_text_candidate(value) {
            candidates.insert(value.to_string());
        }
    }
    candidates.into_iter().collect()
}

fn prioritize_command_text_candidates(candidates: &[String]) -> Vec<String> {
    let mut prioritized = candidates
        .iter()
        .filter(|candidate| {
            let lower = candidate.to_ascii_lowercase();
            lower.contains("rtp")
                || lower.contains("random")
                || lower.contains("teleport")
                || lower.contains("wild")
                || lower.contains("spawn")
                || lower.contains("survival")
                || lower.contains("overworld")
                || lower.contains("region")
                || lower.contains("command")
                || lower.contains("permission")
                || lower.contains("unknown")
                || lower.contains("error")
                || lower.contains("fail")
        })
        .take(80)
        .cloned()
        .collect::<Vec<_>>();

    if prioritized.is_empty() {
        prioritized = candidates
            .iter()
            .filter(|candidate| is_command_name_candidate(candidate))
            .take(80)
            .cloned()
            .collect();
    }

    prioritized
}

fn is_loose_packet_text_candidate(value: &str) -> bool {
    if value.len() < 2 || value.len() > 160 {
        return false;
    }
    if value
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return false;
    }
    value.chars().any(|ch| ch.is_ascii_alphabetic())
}

fn is_command_name_candidate(value: &str) -> bool {
    let value = value.trim_start_matches('/');
    (2..=32).contains(&value.len())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '.'))
}

fn sanitize_log_text(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect::<String>();
    let compact = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_LEN: usize = 120;
    if compact.chars().count() > MAX_LEN {
        format!("{}...", compact.chars().take(MAX_LEN).collect::<String>())
    } else {
        compact
    }
}

fn count_packet_summaries(packet_stream: &[u8]) -> EngineResult<usize> {
    let mut offset = 0usize;
    let mut count = 0usize;
    while read_packet_summary(packet_stream, &mut offset)?.is_some() {
        count += 1;
    }
    Ok(count)
}

/// Parses the policy-relevant fields of a `StartGame` payload (the bytes
/// after the packet header).
pub fn observe_start_game(payload: &[u8]) -> Option<ObservedStartGame> {
    read_start_game_observation(payload, 0)
}

fn read_start_game_observation(packet: &[u8], payload_offset: usize) -> Option<ObservedStartGame> {
    let mut offset = payload_offset;
    let entity_id = read_trace_zigzag_i64(packet, &mut offset)?;
    let runtime_id = read_trace_var_u64(packet, &mut offset)?;
    read_trace_var_u32(packet, &mut offset)?;
    let position = read_trace_vec3f(packet, &mut offset).ok()?;
    let rotation = read_trace_vec2f(packet, &mut offset).ok()?;
    let policy = read_start_game_policy_observation(packet, payload_offset);
    Some(ObservedStartGame {
        entity_id,
        runtime_id,
        position,
        rotation,
        disable_player_interactions: policy
            .as_ref()
            .map(|policy| policy.disable_player_interactions),
        server_authoritative_block_breaking: policy
            .as_ref()
            .map(|policy| policy.server_authoritative_block_breaking),
        block_network_ids_are_hashes: policy
            .as_ref()
            .map(|policy| policy.block_network_ids_are_hashes),
        block_property_count: policy.map(|policy| policy.block_property_count),
    })
}

#[derive(Debug, Clone)]
struct ObservedStartGamePolicy {
    disable_player_interactions: bool,
    server_authoritative_block_breaking: bool,
    block_network_ids_are_hashes: bool,
    block_property_count: u32,
}

fn read_start_game_policy_observation(
    packet: &[u8],
    payload_offset: usize,
) -> Option<ObservedStartGamePolicy> {
    let mut cursor = StartGamePolicyCursor::new(packet, payload_offset);
    cursor.zigzag64()?;
    cursor.var_u64()?;
    cursor.zigzag32()?;
    cursor.skip(12)?;
    cursor.skip(8)?;
    cursor.skip(8)?;
    cursor.skip(2)?;
    cursor.string()?;
    cursor.zigzag32()?;
    cursor.zigzag32()?;
    cursor.zigzag32()?;
    cursor.bool()?;
    cursor.zigzag32()?;
    cursor.block_coordinates()?;
    cursor.bool()?;
    cursor.zigzag32()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.zigzag32()?;
    cursor.zigzag32()?;
    cursor.bool()?;
    cursor.string()?;
    cursor.skip(4)?;
    cursor.skip(4)?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.var_u32()?;
    cursor.var_u32()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.gamerules()?;
    cursor.experiments()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.var_u32()?;
    cursor.skip(4)?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.bool()?;
    cursor.string()?;
    cursor.skip(4)?;
    cursor.skip(4)?;
    cursor.bool()?;
    cursor.education_shared_resource_uri()?;
    cursor.bool()?;
    cursor.u8()?;
    let disable_player_interactions = cursor.bool()?;
    cursor.string()?;
    cursor.string()?;
    cursor.string()?;
    cursor.string()?;
    cursor.string()?;
    cursor.string()?;
    cursor.string()?;
    cursor.bool()?;
    cursor.zigzag32()?;
    let server_authoritative_block_breaking = cursor.bool()?;
    cursor.skip(8)?;
    cursor.zigzag32()?;
    let block_property_count = cursor.block_properties()?;
    cursor.string()?;
    cursor.bool()?;
    cursor.string()?;
    cursor.nbt()?;
    cursor.skip(8)?;
    cursor.skip(16)?;
    cursor.bool()?;
    let block_network_ids_are_hashes = cursor.bool()?;
    Some(ObservedStartGamePolicy {
        disable_player_interactions,
        server_authoritative_block_breaking,
        block_network_ids_are_hashes,
        block_property_count,
    })
}

fn trace_known_packet_impl(summary: &PacketSummary<'_>) {
    let Some(name) = implemented_packet_name(summary.packet_id) else {
        return;
    };

    let mut offset = summary.payload_offset;
    let result = match summary.packet_id {
        0x3a => decode_level_chunk_boundary(summary.packet, &mut offset),
        0x15 => decode_update_block_boundary(summary.packet, &mut offset),
        0x28 => decode_set_entity_motion_boundary(summary.packet, &mut offset),
        0x2b => decode_set_spawn_position_boundary(summary.packet, &mut offset),
        0x2d => decode_respawn_boundary(summary.packet, &mut offset),
        0x3b | 0x3c | 0x46 => read_trace_var_u32(summary.packet, &mut offset)
            .ok_or_else(|| "invalid varint payload".to_string())
            .map(|_| ()),
        0x48 => decode_game_rules_changed_boundary(summary.packet, &mut offset),
        0x58 => decode_set_title_boundary(summary.packet, &mut offset),
        0x6a => skip_trace_string(summary.packet, &mut offset),
        0x6b => decode_set_display_objective_boundary(summary.packet, &mut offset),
        0x6c => decode_set_score_boundary(summary.packet, &mut offset),
        0x72 => {
            TorchFlowerUpdateSoftEnumPacket::deserialize(summary.packet, summary.payload_offset)
                .map(|_| {
                    offset = summary.packet.len();
                })
        }
        0xa5 => skip_trace_nbt(summary.packet, &mut offset),
        0xbb => decode_update_abilities_boundary(summary.packet, &mut offset),
        0xbc => skip_trace_bytes(summary.packet, &mut offset, 5),
        0xc7 => decode_unlocked_recipes_boundary(summary.packet, &mut offset),
        0x134 => decode_set_hud_boundary(summary.packet, &mut offset),
        0x146 => decode_player_location_boundary(summary.packet, &mut offset),
        _ => Ok(()),
    };

    match result {
        Ok(()) => emit_packet_impl(summary.packet_id, name, summary.packet, offset),
        Err(error) => eprintln!(
            "[PACKET_IMPL_ERROR] id={} name={} offset={} remaining={} error={} hex_dump={}",
            summary.packet_id,
            name,
            offset.saturating_sub(summary.payload_offset),
            summary.packet.len().saturating_sub(offset),
            error,
            hex_dump(&summary.packet[offset..summary.packet.len().min(offset + 64)])
        ),
    }
}

fn emit_packet_impl(packet_id: u32, name: &'static str, packet: &[u8], offset: usize) {
    let remaining = packet.len().saturating_sub(offset);
    eprintln!(
        "[PACKET_IMPL] id={} name={} offset={} remaining={}",
        packet_id, name, offset, remaining
    );
    if remaining > 0 {
        eprintln!(
            "[PACKET_IMPL_WARN] id={} name={} trailing_bytes={}",
            packet_id,
            name,
            hex_dump(&packet[offset..packet.len().min(offset + 64)])
        );
    }
}

fn decode_level_chunk_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid chunk x".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid chunk z".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid dimension".to_string())?;
    let sub_chunk_count =
        read_trace_var_i32(packet, offset).ok_or_else(|| "invalid sub_chunk_count".to_string())?;
    if sub_chunk_count == -2 {
        skip_trace_bytes(packet, offset, 2)?;
    }
    let cache_enabled = read_trace_bool(packet, offset)?;
    if cache_enabled {
        let blob_count = read_trace_var_u32(packet, offset)
            .ok_or_else(|| "invalid cache blob count".to_string())?;
        skip_trace_bytes(packet, offset, blob_count as usize * 8)?;
    }
    let payload_len = read_trace_var_u32(packet, offset)
        .ok_or_else(|| "invalid chunk payload length".to_string())? as usize;
    skip_trace_bytes(packet, offset, payload_len)
}

fn decode_update_block_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    read_trace_zigzag_i32(packet, offset).ok_or_else(|| "invalid block x".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid block y".to_string())?;
    read_trace_zigzag_i32(packet, offset).ok_or_else(|| "invalid block z".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid block_runtime_id".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid flags".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid layer".to_string())?;
    Ok(())
}

fn decode_game_rules_changed_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    let count =
        read_trace_var_u32(packet, offset).ok_or_else(|| "invalid gamerule count".to_string())?;
    for _ in 0..count {
        skip_trace_string(packet, offset)?;
        skip_trace_bytes(packet, offset, 1)?;
        let rule_type = read_trace_var_u32(packet, offset)
            .ok_or_else(|| "invalid gamerule type".to_string())?;
        match rule_type {
            1 => skip_trace_bytes(packet, offset, 1)?,
            2 => skip_trace_bytes(packet, offset, 4)?,
            3 => skip_trace_bytes(packet, offset, 4)?,
            other => return Err(format!("unknown gamerule type {other}")),
        }
    }
    Ok(())
}

fn decode_set_entity_motion_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    read_trace_var_u64(packet, offset).ok_or_else(|| "invalid runtime_entity_id".to_string())?;
    skip_trace_bytes(packet, offset, 12)?;
    read_trace_var_u64(packet, offset).ok_or_else(|| "invalid tick".to_string())?;
    Ok(())
}

fn decode_set_spawn_position_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid spawn_type".to_string())?;
    skip_trace_block_coordinates(packet, offset)?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid dimension".to_string())?;
    skip_trace_block_coordinates(packet, offset)
}

fn decode_respawn_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    skip_trace_bytes(packet, offset, 12)?;
    skip_trace_bytes(packet, offset, 1)?;
    read_trace_var_u64(packet, offset).ok_or_else(|| "invalid runtime_entity_id".to_string())?;
    Ok(())
}

fn decode_set_title_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid title type".to_string())?;
    skip_trace_string(packet, offset)?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid fade_in_time".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid stay_time".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid fade_out_time".to_string())?;
    skip_trace_string(packet, offset)?;
    skip_trace_string(packet, offset)?;
    skip_trace_string(packet, offset)
}

fn decode_set_score_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    let action = *packet
        .get(*offset)
        .ok_or_else(|| "missing set_score action".to_string())?;
    *offset += 1;

    let count =
        read_trace_var_u32(packet, offset).ok_or_else(|| "invalid set_score count".to_string())?;
    for _ in 0..count {
        read_trace_var_u64(packet, offset).ok_or_else(|| "invalid scoreboard_id".to_string())?;
        skip_trace_string(packet, offset)?;
        skip_trace_bytes(packet, offset, 4)?;

        if action == 0 {
            let entry_type = *packet
                .get(*offset)
                .ok_or_else(|| "missing set_score entry_type".to_string())?;
            *offset += 1;
            match entry_type {
                1 | 2 => {
                    read_trace_var_u64(packet, offset)
                        .ok_or_else(|| "invalid set_score entity_unique_id".to_string())?;
                }
                3 => skip_trace_string(packet, offset)?,
                other => return Err(format!("unknown set_score entry_type {other}")),
            }
        } else if action != 1 {
            return Err(format!("unknown set_score action {action}"));
        }
    }

    Ok(())
}

fn decode_set_display_objective_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    skip_trace_string(packet, offset)?;
    skip_trace_string(packet, offset)?;
    skip_trace_string(packet, offset)?;
    skip_trace_string(packet, offset)?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid sort_order".to_string())?;
    Ok(())
}

fn decode_update_abilities_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    skip_trace_bytes(packet, offset, 8)?;
    skip_trace_bytes(packet, offset, 1)?;
    skip_trace_bytes(packet, offset, 1)?;
    let layer_count = *packet
        .get(*offset)
        .ok_or_else(|| "missing ability layer count".to_string())? as usize;
    *offset += 1;
    for _ in 0..layer_count {
        skip_trace_bytes(packet, offset, 2)?;
        skip_trace_bytes(packet, offset, 4)?;
        skip_trace_bytes(packet, offset, 4)?;
        skip_trace_bytes(packet, offset, 4)?;
        skip_trace_bytes(packet, offset, 4)?;
        skip_trace_bytes(packet, offset, 4)?;
    }
    Ok(())
}

fn decode_unlocked_recipes_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    skip_trace_bytes(packet, offset, 4)?;
    let count =
        read_trace_var_u32(packet, offset).ok_or_else(|| "invalid recipe count".to_string())?;
    for _ in 0..count {
        skip_trace_string(packet, offset)?;
    }
    Ok(())
}

fn decode_set_hud_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    let count = read_trace_var_u32(packet, offset)
        .ok_or_else(|| "invalid HUD element count".to_string())?;
    for _ in 0..count {
        read_trace_var_u32(packet, offset).ok_or_else(|| "invalid HUD element".to_string())?;
    }
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid HUD visibility".to_string())?;
    Ok(())
}

fn decode_player_location_boundary(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    let location_type = read_trace_i32_le(packet, offset)?;
    read_trace_var_u64(packet, offset).ok_or_else(|| "invalid entity_unique_id".to_string())?;
    if location_type == 0 {
        skip_trace_bytes(packet, offset, 12)?;
    }
    Ok(())
}

fn skip_trace_block_coordinates(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid block x".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid block y".to_string())?;
    read_trace_var_u32(packet, offset).ok_or_else(|| "invalid block z".to_string())?;
    Ok(())
}

fn read_trace_bool(packet: &[u8], offset: &mut usize) -> Result<bool, String> {
    let value = *packet
        .get(*offset)
        .ok_or_else(|| "missing bool".to_string())?;
    *offset += 1;
    Ok(value != 0)
}

fn skip_trace_string(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    let len = read_trace_var_u32(packet, offset)
        .ok_or_else(|| "invalid string length".to_string())? as usize;
    skip_trace_bytes(packet, offset, len)
}

fn read_trace_string(packet: &[u8], offset: &mut usize) -> Option<String> {
    let len = read_trace_var_u32(packet, offset)? as usize;
    let end = offset.checked_add(len)?;
    let bytes = packet.get(*offset..end)?;
    *offset = end;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn filter_locally_decoded_packets(
    packet_stream: &[u8],
) -> EngineResult<(Vec<u8>, Vec<ObservedPacket>)> {
    let mut offset = 0usize;
    let mut filtered = Vec::with_capacity(packet_stream.len());
    let mut observed = Vec::new();

    while let Some(summary) = read_packet_summary(packet_stream, &mut offset)? {
        if summary.packet_id == 0x72 {
            TorchFlowerUpdateSoftEnumPacket::deserialize(summary.packet, summary.payload_offset)
                .map_err(|err| {
                    EngineError::Bedrock(format!("decode UpdateSoftEnumPacket: {err}"))
                })?;
            observed.push(ObservedPacket::UpdateSoftEnum);
            continue;
        }

        write_unsigned_varint_u32(summary.packet.len() as u32, &mut filtered);
        filtered.extend_from_slice(summary.packet);
    }

    Ok((filtered, observed))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorchFlowerUpdateSoftEnumPacket {
    pub enum_name: String,
    pub options: Vec<String>,
    pub action_type: TorchFlowerSoftEnumAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorchFlowerSoftEnumAction {
    Add,
    Remove,
    Update,
}

impl TorchFlowerSoftEnumAction {
    fn from_u8(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(Self::Add),
            1 => Ok(Self::Remove),
            2 => Ok(Self::Update),
            other => Err(format!("unknown action_type {other}")),
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Add => 0,
            Self::Remove => 1,
            Self::Update => 2,
        }
    }
}

impl TorchFlowerUpdateSoftEnumPacket {
    pub fn serialize(&self, out: &mut Vec<u8>) {
        write_trace_string(&self.enum_name, out);
        write_unsigned_varint_u32(self.options.len() as u32, out);
        for option in &self.options {
            write_trace_string(option, out);
        }
        out.push(self.action_type.as_u8());
    }

    pub fn deserialize(packet: &[u8], payload_offset: usize) -> Result<Self, String> {
        Self::decode(packet, payload_offset, false)
    }

    pub fn deserialize_with_trace(packet: &[u8], payload_offset: usize) -> Result<Self, String> {
        Self::decode(packet, payload_offset, true)
    }

    fn decode(packet: &[u8], payload_offset: usize, trace: bool) -> Result<Self, String> {
        let mut cursor = UpdateSoftEnumTraceCursor::new(packet, payload_offset, trace);
        if trace {
            eprintln!(
                "[UPDATE_SOFT_ENUM_DECODE] packet_id=114 payload_len={}",
                cursor.payload_len()
            );
        }

        let result: Result<Self, String> = (|| {
            let enum_name = cursor.string("enum_name")?;
            let option_count = cursor.var_u32("option_count")?;
            let mut options = Vec::with_capacity(option_count as usize);
            for index in 0..option_count {
                options.push(cursor.indexed_string("option", index)?);
            }
            let action_type = TorchFlowerSoftEnumAction::from_u8(cursor.u8("action_type")?)?;
            Ok(Self {
                enum_name,
                options,
                action_type,
            })
        })();

        match result {
            Ok(packet) => {
                if trace {
                    eprintln!(
                        "[UPDATE_SOFT_ENUM_BOUNDARY] payload_len={} reader_offset={} remaining={}",
                        cursor.payload_len(),
                        cursor.payload_offset(),
                        cursor.remaining()
                    );
                }
                Ok(packet)
            }
            Err(error) => {
                if trace {
                    cursor.error(cursor.current_field(), error.clone());
                }
                Err(error)
            }
        }
    }
}

struct UpdateSoftEnumTraceCursor<'a> {
    packet: &'a [u8],
    offset: usize,
    payload_start: usize,
    field: String,
    trace: bool,
}

impl<'a> UpdateSoftEnumTraceCursor<'a> {
    fn new(packet: &'a [u8], payload_start: usize, trace: bool) -> Self {
        Self {
            packet,
            offset: payload_start,
            payload_start,
            field: "start".to_string(),
            trace,
        }
    }

    fn payload_len(&self) -> usize {
        self.packet.len().saturating_sub(self.payload_start)
    }

    fn payload_offset(&self) -> usize {
        self.offset.saturating_sub(self.payload_start)
    }

    fn remaining(&self) -> usize {
        self.packet.len().saturating_sub(self.offset)
    }

    fn current_field(&self) -> &str {
        &self.field
    }

    fn log_field(&mut self, field: impl Into<String>, before: usize) {
        if self.trace {
            eprintln!(
                "[UPDATE_SOFT_ENUM_DECODE] field={} offset_before={} offset_after={} remaining={}",
                field.into(),
                before.saturating_sub(self.payload_start),
                self.offset.saturating_sub(self.payload_start),
                self.remaining()
            );
        }
    }

    fn error(&self, field: &str, error: String) {
        eprintln!(
            "[UPDATE_SOFT_ENUM_ERROR] field={} offset={} remaining={} total_payload_len={}\nhex_dump: {}\nerror: {}",
            field,
            self.payload_offset(),
            self.remaining(),
            self.payload_len(),
            hex_dump(&self.packet[self.offset..self.packet.len().min(self.offset + 64)]),
            error
        );
    }

    fn var_u32(&mut self, field: &'static str) -> Result<u32, String> {
        self.field = field.to_string();
        let before = self.offset;
        let value = read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid var_u32".to_string())?;
        self.log_field(field, before);
        Ok(value)
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, String> {
        self.field = field.to_string();
        let before = self.offset;
        let value = *self
            .packet
            .get(self.offset)
            .ok_or_else(|| "need 1 byte".to_string())?;
        self.offset += 1;
        self.log_field(field, before);
        Ok(value)
    }

    fn string(&mut self, field: &'static str) -> Result<String, String> {
        self.field = field.to_string();
        let before = self.offset;
        let len = read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid string length".to_string())? as usize;
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.packet.len())
            .ok_or_else(|| format!("string needs {len} bytes"))?;
        let value = std::str::from_utf8(&self.packet[self.offset..end])
            .map_err(|err| format!("invalid UTF-8 string: {err}"))?
            .to_string();
        self.offset = end;
        self.log_field(field, before);
        Ok(value)
    }

    fn indexed_string(&mut self, prefix: &'static str, index: u32) -> Result<String, String> {
        let field = format!("{prefix}[{index}]");
        self.field = field.clone();
        let before = self.offset;
        let len = read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid string length".to_string())? as usize;
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.packet.len())
            .ok_or_else(|| format!("string needs {len} bytes"))?;
        let value = std::str::from_utf8(&self.packet[self.offset..end])
            .map_err(|err| format!("invalid UTF-8 string: {err}"))?
            .to_string();
        self.offset = end;
        self.log_field(field, before);
        Ok(value)
    }
}

fn write_trace_string(value: &str, out: &mut Vec<u8>) {
    write_unsigned_varint_u32(value.len() as u32, out);
    out.extend_from_slice(value.as_bytes());
}

fn trace_start_game_packet(packet: &[u8], payload_offset: usize) {
    let mut cursor = StartGameTraceCursor::new(packet, payload_offset);
    eprintln!(
        "[STARTGAME_DECODE] packet_id=11 payload_len={}",
        packet.len().saturating_sub(payload_offset)
    );

    let result: Result<(), String> = (|| {
        cursor.zigzag64("entity_id")?;
        cursor.var_u64("runtime_entity_id")?;
        cursor.zigzag32("player_gamemode")?;
        cursor.skip("player_position", 12)?;
        cursor.skip("rotation", 8)?;
        cursor.le_u64("seed")?;
        cursor.skip("biome_type", 2)?;
        cursor.string("biome_name")?;
        cursor.zigzag32("dimension")?;
        cursor.zigzag32("generator")?;
        cursor.zigzag32("world_gamemode")?;
        cursor.bool("hardcore")?;
        cursor.zigzag32("difficulty")?;
        cursor.block_coordinates("spawn_position")?;
        cursor.bool("achievements_disabled")?;
        cursor.zigzag32("editor_world_type")?;
        cursor.bool("created_in_editor")?;
        cursor.bool("exported_from_editor")?;
        cursor.zigzag32("day_cycle_stop_time")?;
        cursor.zigzag32("edu_offer")?;
        cursor.bool("edu_features_enabled")?;
        cursor.string("edu_product_uuid")?;
        cursor.skip("rain_level", 4)?;
        cursor.skip("lightning_level", 4)?;
        cursor.bool("has_confirmed_platform_locked_content")?;
        cursor.bool("is_multiplayer")?;
        cursor.bool("broadcast_to_lan")?;
        cursor.var_u32("xbox_live_broadcast_mode")?;
        cursor.var_u32("platform_broadcast_mode")?;
        cursor.bool("enable_commands")?;
        cursor.bool("is_texturepacks_required")?;
        cursor.gamerules("gamerules")?;
        cursor.experiments("experiments")?;
        cursor.bool("experiments_previously_used")?;
        cursor.bool("bonus_chest")?;
        cursor.bool("map_enabled")?;
        cursor.var_u32("permission_level")?;
        cursor.skip("server_chunk_tick_range", 4)?;
        cursor.bool("has_locked_behavior_pack")?;
        cursor.bool("has_locked_resource_pack")?;
        cursor.bool("is_from_locked_world_template")?;
        cursor.bool("msa_gamertags_only")?;
        cursor.bool("is_from_world_template")?;
        cursor.bool("is_world_template_option_locked")?;
        cursor.bool("only_spawn_v1_villagers")?;
        cursor.bool("persona_disabled")?;
        cursor.bool("custom_skins_disabled")?;
        cursor.bool("emote_chat_muted")?;
        cursor.string("game_version")?;
        cursor.skip("limited_world_width", 4)?;
        cursor.skip("limited_world_length", 4)?;
        cursor.bool("is_new_nether")?;
        cursor.education_shared_resource_uri("edu_resource_uri")?;
        cursor.bool("experimental_gameplay_override")?;
        cursor.u8("chat_restriction_level")?;
        cursor.bool("disable_player_interactions")?;
        cursor.string("server_identifier")?;
        cursor.string("world_identifier")?;
        cursor.string("scenario_identifier")?;
        cursor.string("owner_identifier")?;
        cursor.string("level_id")?;
        cursor.string("world_name")?;
        cursor.string("premium_world_template_id")?;
        cursor.bool("is_trial")?;
        cursor.zigzag32("rewind_history_size")?;
        cursor.bool("server_authoritative_block_breaking")?;
        cursor.skip("current_tick", 8)?;
        cursor.zigzag32("enchantment_seed")?;
        cursor.block_properties("block_properties")?;
        cursor.string("multiplayer_correlation_id")?;
        cursor.bool("server_authoritative_inventory")?;
        cursor.string("engine")?;
        cursor.nbt("property_data")?;
        cursor.le_u64("block_pallette_checksum")?;
        cursor.skip("world_template_id", 16)?;
        cursor.bool("client_side_generation")?;
        cursor.bool("block_network_ids_are_hashes")?;
        cursor.bool("server_controlled_sound")?;
        Ok(())
    })();

    if let Err(error) = result {
        cursor.error(cursor.current_field(), error);
        return;
    }

    eprintln!(
        "[STARTGAME_DECODE] complete offset={} remaining={}",
        cursor.offset,
        cursor.remaining()
    );
    if cursor.remaining() > 0 {
        eprintln!(
            "[STARTGAME_DECODE] trailing_bytes={}",
            hex_dump(&cursor.packet[cursor.offset..cursor.packet.len().min(cursor.offset + 64)])
        );
    }
}

struct StartGamePolicyCursor<'a> {
    packet: &'a [u8],
    offset: usize,
}

impl<'a> StartGamePolicyCursor<'a> {
    fn new(packet: &'a [u8], offset: usize) -> Self {
        Self { packet, offset }
    }

    fn skip(&mut self, len: usize) -> Option<()> {
        let end = self.offset.checked_add(len)?;
        if end > self.packet.len() {
            return None;
        }
        self.offset = end;
        Some(())
    }

    fn u8(&mut self) -> Option<u8> {
        let value = *self.packet.get(self.offset)?;
        self.offset += 1;
        Some(value)
    }

    fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }

    fn var_u32(&mut self) -> Option<u32> {
        read_trace_var_u32(self.packet, &mut self.offset)
    }

    fn var_u64(&mut self) -> Option<u64> {
        read_trace_var_u64(self.packet, &mut self.offset)
    }

    fn zigzag32(&mut self) -> Option<i32> {
        self.var_u32()
            .map(|value| ((value >> 1) as i32) ^ (-((value & 1) as i32)))
    }

    fn zigzag64(&mut self) -> Option<i64> {
        self.var_u64()
            .map(|value| ((value >> 1) as i64) ^ (-((value & 1) as i64)))
    }

    fn string(&mut self) -> Option<()> {
        let len = self.var_u32()? as usize;
        self.skip(len)
    }

    fn block_coordinates(&mut self) -> Option<()> {
        self.zigzag32()?;
        self.var_u32()?;
        self.var_u32()?;
        Some(())
    }

    fn gamerules(&mut self) -> Option<()> {
        let count = self.var_u32()?;
        for _ in 0..count {
            self.string()?;
            self.bool()?;
            let rule_type = self.var_u32()?;
            match rule_type {
                1 => {
                    self.bool()?;
                }
                2 => {
                    self.var_u32()?;
                }
                3 => {
                    self.skip(4)?;
                }
                _ => return None,
            }
        }
        Some(())
    }

    fn experiments(&mut self) -> Option<()> {
        let count = self.take_i32_le()?;
        if count < 0 {
            return None;
        }
        for _ in 0..count {
            self.string()?;
            self.bool()?;
        }
        Some(())
    }

    fn education_shared_resource_uri(&mut self) -> Option<()> {
        self.string()?;
        self.string()
    }

    fn block_properties(&mut self) -> Option<u32> {
        let count = self.var_u32()?;
        for _ in 0..count {
            self.string()?;
            self.nbt()?;
        }
        Some(count)
    }

    fn nbt(&mut self) -> Option<()> {
        skip_trace_nbt(self.packet, &mut self.offset).ok()
    }

    fn take_i32_le(&mut self) -> Option<i32> {
        let end = self.offset.checked_add(4)?;
        if end > self.packet.len() {
            return None;
        }
        let bytes: [u8; 4] = self.packet[self.offset..end].try_into().ok()?;
        self.offset = end;
        Some(i32::from_le_bytes(bytes))
    }
}

struct StartGameTraceCursor<'a> {
    packet: &'a [u8],
    offset: usize,
    field: &'static str,
}

impl<'a> StartGameTraceCursor<'a> {
    fn new(packet: &'a [u8], offset: usize) -> Self {
        Self {
            packet,
            offset,
            field: "start",
        }
    }

    fn current_field(&self) -> &'static str {
        self.field
    }

    fn remaining(&self) -> usize {
        self.packet.len().saturating_sub(self.offset)
    }

    fn log_field(&mut self, field: &'static str) {
        self.field = field;
        eprintln!(
            "[STARTGAME_FIELD] field={} offset={} remaining={}",
            field,
            self.offset,
            self.remaining()
        );
    }

    fn error(&self, field: &'static str, error: String) {
        eprintln!(
            "[STARTGAME_ERROR] field={} offset={} remaining={}\nhex_dump: {}\nerror: {}",
            field,
            self.offset,
            self.remaining(),
            hex_dump(&self.packet[self.offset..self.packet.len().min(self.offset + 64)]),
            error
        );
    }

    fn take(&mut self, field: &'static str, len: usize) -> Result<&'a [u8], String> {
        self.log_field(field);
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.packet.len())
            .ok_or_else(|| format!("need {len} bytes"))?;
        let bytes = &self.packet[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn skip(&mut self, field: &'static str, len: usize) -> Result<(), String> {
        self.take(field, len).map(|_| ())
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, String> {
        Ok(self.take(field, 1)?[0])
    }

    fn bool(&mut self, field: &'static str) -> Result<bool, String> {
        Ok(self.u8(field)? != 0)
    }

    fn le_u64(&mut self, field: &'static str) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .take(field, 8)?
            .try_into()
            .map_err(|_| "invalid u64".to_string())?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn var_u32(&mut self, field: &'static str) -> Result<u32, String> {
        self.log_field(field);
        read_trace_var_u32(self.packet, &mut self.offset).ok_or_else(|| "invalid var_u32".into())
    }

    fn var_u64(&mut self, field: &'static str) -> Result<u64, String> {
        self.log_field(field);
        read_trace_var_u64(self.packet, &mut self.offset).ok_or_else(|| "invalid var_u64".into())
    }

    fn zigzag32(&mut self, field: &'static str) -> Result<i32, String> {
        let value = self.var_u32(field)?;
        Ok(((value >> 1) as i32) ^ (-((value & 1) as i32)))
    }

    fn zigzag64(&mut self, field: &'static str) -> Result<i64, String> {
        let value = self.var_u64(field)?;
        Ok(((value >> 1) as i64) ^ (-((value & 1) as i64)))
    }

    fn string(&mut self, field: &'static str) -> Result<(), String> {
        self.log_field(field);
        let len = read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid string length".to_string())? as usize;
        self.skip_bytes(len)
    }

    fn block_coordinates(&mut self, field: &'static str) -> Result<(), String> {
        self.log_field(field);
        read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid block x".to_string())?;
        read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid block y".to_string())?;
        read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid block z".to_string())?;
        Ok(())
    }

    fn gamerules(&mut self, field: &'static str) -> Result<(), String> {
        self.log_field(field);
        let count = read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid gamerule count".to_string())?;
        for _ in 0..count {
            self.string("gamerules.name")?;
            self.bool("gamerules.editable")?;
            let rule_type = self.var_u32("gamerules.type")?;
            match rule_type {
                1 => {
                    self.bool("gamerules.value.bool")?;
                }
                2 => {
                    self.var_u32("gamerules.value.int")?;
                }
                3 => {
                    self.skip("gamerules.value.float", 4)?;
                }
                other => return Err(format!("unknown gamerule type {other}")),
            }
        }
        Ok(())
    }

    fn experiments(&mut self, field: &'static str) -> Result<(), String> {
        self.log_field(field);
        let count = self.take_i32_le("experiments.count")?;
        if count < 0 {
            return Err(format!("negative experiment count {count}"));
        }
        for _ in 0..count {
            self.string("experiments.name")?;
            self.bool("experiments.enabled")?;
        }
        Ok(())
    }

    fn education_shared_resource_uri(&mut self, field: &'static str) -> Result<(), String> {
        self.log_field(field);
        self.string("edu_resource_uri.button_name")?;
        self.string("edu_resource_uri.link_uri")
    }

    fn block_properties(&mut self, field: &'static str) -> Result<(), String> {
        self.log_field(field);
        let count = read_trace_var_u32(self.packet, &mut self.offset)
            .ok_or_else(|| "invalid block property count".to_string())?;
        for _ in 0..count {
            self.string("block_properties.name")?;
            self.nbt("block_properties.state")?;
        }
        Ok(())
    }

    fn nbt(&mut self, field: &'static str) -> Result<(), String> {
        self.log_field(field);
        skip_trace_nbt(self.packet, &mut self.offset)
    }

    fn take_i32_le(&mut self, field: &'static str) -> Result<i32, String> {
        let bytes: [u8; 4] = self
            .take(field, 4)?
            .try_into()
            .map_err(|_| "invalid i32".to_string())?;
        Ok(i32::from_le_bytes(bytes))
    }

    fn skip_bytes(&mut self, len: usize) -> Result<(), String> {
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.packet.len())
            .ok_or_else(|| format!("need {len} bytes"))?;
        self.offset = end;
        Ok(())
    }
}

fn read_trace_var_u32(packet: &[u8], offset: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        let byte = *packet.get(*offset)?;
        *offset += 1;
        value |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn read_trace_var_i32(packet: &[u8], offset: &mut usize) -> Option<i32> {
    read_trace_var_u32(packet, offset).map(|value| value as i32)
}

fn read_trace_zigzag_i32(packet: &[u8], offset: &mut usize) -> Option<i32> {
    read_trace_var_u32(packet, offset).map(|value| ((value >> 1) as i32) ^ (-((value & 1) as i32)))
}

fn read_trace_zigzag_i64(packet: &[u8], offset: &mut usize) -> Option<i64> {
    read_trace_var_u64(packet, offset).map(|value| ((value >> 1) as i64) ^ (-((value & 1) as i64)))
}

fn read_trace_var_u64(packet: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for _ in 0..10 {
        let byte = *packet.get(*offset)?;
        *offset += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn skip_trace_nbt(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    let tag = *packet
        .get(*offset)
        .ok_or_else(|| "missing NBT tag".to_string())?;
    *offset += 1;
    if tag == 0 {
        return Ok(());
    }
    skip_trace_nbt_string(packet, offset)?;
    skip_trace_nbt_payload(packet, offset, tag)
}

fn skip_trace_nbt_payload(packet: &[u8], offset: &mut usize, tag: u8) -> Result<(), String> {
    match tag {
        0 => Ok(()),
        1 => skip_trace_bytes(packet, offset, 1),
        2 | 3 => read_trace_var_u32(packet, offset)
            .ok_or_else(|| "invalid NBT varint payload".to_string())
            .map(|_| ()),
        4 => read_trace_var_u64(packet, offset)
            .ok_or_else(|| "invalid NBT varint64 payload".to_string())
            .map(|_| ()),
        5 => skip_trace_bytes(packet, offset, 4),
        6 => skip_trace_bytes(packet, offset, 8),
        7 => {
            let len = read_trace_var_u32(packet, offset)
                .ok_or_else(|| "invalid NBT byte array length".to_string())?;
            skip_trace_bytes(packet, offset, len as usize)
        }
        8 => skip_trace_nbt_string(packet, offset),
        9 => {
            let item_tag = *packet
                .get(*offset)
                .ok_or_else(|| "missing NBT list item tag".to_string())?;
            *offset += 1;
            let len = read_trace_var_u32(packet, offset)
                .ok_or_else(|| "invalid NBT list length".to_string())?;
            for _ in 0..len {
                skip_trace_nbt_payload(packet, offset, item_tag)?;
            }
            Ok(())
        }
        10 => loop {
            let child_tag = *packet
                .get(*offset)
                .ok_or_else(|| "missing NBT compound child tag".to_string())?;
            *offset += 1;
            if child_tag == 0 {
                break Ok(());
            }
            skip_trace_nbt_string(packet, offset)?;
            skip_trace_nbt_payload(packet, offset, child_tag)?;
        },
        11 => {
            let len = read_trace_var_u32(packet, offset)
                .ok_or_else(|| "invalid NBT int array length".to_string())?;
            for _ in 0..len {
                read_trace_var_u32(packet, offset)
                    .ok_or_else(|| "invalid NBT int array item".to_string())?;
            }
            Ok(())
        }
        12 => {
            let len = read_trace_var_u32(packet, offset)
                .ok_or_else(|| "invalid NBT long array length".to_string())?;
            for _ in 0..len {
                read_trace_var_u64(packet, offset)
                    .ok_or_else(|| "invalid NBT long array item".to_string())?;
            }
            Ok(())
        }
        other => Err(format!("unknown NBT tag {other}")),
    }
}

fn skip_trace_nbt_string(packet: &[u8], offset: &mut usize) -> Result<(), String> {
    let len = read_trace_var_u32(packet, offset)
        .ok_or_else(|| "invalid NBT string length".to_string())? as usize;
    skip_trace_bytes(packet, offset, len)
}

fn read_trace_i32_le(packet: &[u8], offset: &mut usize) -> Result<i32, String> {
    let end = offset
        .checked_add(4)
        .filter(|end| *end <= packet.len())
        .ok_or_else(|| "need 4 bytes".to_string())?;
    let bytes: [u8; 4] = packet[*offset..end]
        .try_into()
        .map_err(|_| "invalid i32".to_string())?;
    *offset = end;
    Ok(i32::from_le_bytes(bytes))
}

fn read_trace_f32_le(packet: &[u8], offset: &mut usize) -> Result<f32, String> {
    let end = offset
        .checked_add(4)
        .filter(|end| *end <= packet.len())
        .ok_or_else(|| "need 4 bytes".to_string())?;
    let bytes: [u8; 4] = packet[*offset..end]
        .try_into()
        .map_err(|_| "invalid f32".to_string())?;
    *offset = end;
    Ok(f32::from_le_bytes(bytes))
}

fn read_trace_vec2f(packet: &[u8], offset: &mut usize) -> Result<(f32, f32), String> {
    Ok((
        read_trace_f32_le(packet, offset)?,
        read_trace_f32_le(packet, offset)?,
    ))
}

fn read_trace_vec3f(packet: &[u8], offset: &mut usize) -> Result<(f32, f32, f32), String> {
    Ok((
        read_trace_f32_le(packet, offset)?,
        read_trace_f32_le(packet, offset)?,
        read_trace_f32_le(packet, offset)?,
    ))
}

fn skip_trace_bytes(packet: &[u8], offset: &mut usize, len: usize) -> Result<(), String> {
    let end = offset
        .checked_add(len)
        .filter(|end| *end <= packet.len())
        .ok_or_else(|| format!("need {len} bytes"))?;
    *offset = end;
    Ok(())
}

fn hex_dump(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_trace_position(position: (f32, f32, f32)) -> String {
    format!("{:.3},{:.3},{:.3}", position.0, position.1, position.2)
}

fn read_packet_summary<'a>(
    packet_stream: &'a [u8],
    offset: &mut usize,
) -> EngineResult<Option<PacketSummary<'a>>> {
    if *offset >= packet_stream.len() {
        return Ok(None);
    }

    let packet_len = read_var_u32_from(packet_stream, offset)
        .ok_or_else(|| EngineError::Bedrock("decode batch packet length".to_string()))?
        as usize;
    let end = offset
        .checked_add(packet_len)
        .filter(|end| *end <= packet_stream.len())
        .ok_or_else(|| {
            EngineError::Bedrock(format!(
                "batch packet length {packet_len} exceeds remaining {}",
                packet_stream.len().saturating_sub(*offset)
            ))
        })?;
    let packet = &packet_stream[*offset..end];
    *offset = end;

    let mut packet_offset = 0usize;
    let header = read_var_u32_from(packet, &mut packet_offset)
        .ok_or_else(|| EngineError::Bedrock("decode batch packet header".to_string()))?;
    let packet_id = header & 0x3ff;
    let payload_len = packet_len.saturating_sub(packet_offset);

    Ok(Some(PacketSummary {
        packet,
        packet_id,
        payload_len,
        payload_offset: packet_offset,
    }))
}

fn compression_name(compression: Option<&Compression>) -> &'static str {
    match compression {
        Some(Compression::Zlib { .. }) => "zlib",
        Some(Compression::Snappy { .. }) => "snappy",
        Some(Compression::None) => "none",
        None => "none",
    }
}

fn bedrock_packet_name(packet_id: u32) -> &'static str {
    bedrock_packet_registry_name(packet_id).unwrap_or("Unknown")
}

fn implemented_packet_name(packet_id: u32) -> Option<&'static str> {
    match packet_id {
        0x3a => Some("level_chunk"),
        0x28 => Some("set_entity_motion"),
        0x2b => Some("set_spawn_position"),
        0x2d => Some("respawn"),
        0x3b => Some("set_commands_enabled"),
        0x3c => Some("set_difficulty"),
        0x46 => Some("chunk_radius_update"),
        0x45 => Some("request_chunk_radius"),
        0x48 => Some("game_rules_changed"),
        0x58 => Some("set_title"),
        0x6a => Some("remove_objective"),
        0x6b => Some("set_display_objective"),
        0x6c => Some("set_score"),
        0x72 => Some("update_soft_enum"),
        0xa5 => Some("sync_entity_property"),
        0xbb => Some("update_abilities"),
        0xbc => Some("update_adventure_settings"),
        0xc7 => Some("unlocked_recipes"),
        0x134 => Some("set_hud"),
        0x146 => Some("player_location"),
        _ => None,
    }
}

fn bedrock_packet_registry_name(packet_id: u32) -> Option<&'static str> {
    match packet_id {
        0x01 => Some("login"),
        0x02 => Some("play_status"),
        0x03 => Some("server_to_client_handshake"),
        0x04 => Some("client_to_server_handshake"),
        0x05 => Some("disconnect"),
        0x06 => Some("resource_packs_info"),
        0x07 => Some("resource_pack_stack"),
        0x08 => Some("resource_pack_client_response"),
        0x09 => Some("text"),
        0x0a => Some("set_time"),
        0x0b => Some("start_game"),
        0x0c => Some("add_player"),
        0x0d => Some("add_entity"),
        0x0e => Some("remove_entity"),
        0x0f => Some("add_item_entity"),
        0x11 => Some("take_item_entity"),
        0x12 => Some("move_entity"),
        0x13 => Some("move_player"),
        0x15 => Some("update_block"),
        0x17 => Some("tick_sync"),
        0x1d => Some("update_attributes"),
        0x1e => Some("inventory_transaction"),
        0x1f => Some("mob_equipment"),
        0x20 => Some("mob_armor_equipment"),
        0x27 => Some("set_entity_data"),
        0x28 => Some("set_entity_motion"),
        0x2c => Some("animate"),
        0x2b => Some("set_spawn_position"),
        0x2d => Some("respawn"),
        0x30 => Some("player_hotbar"),
        0x31 => Some("inventory_content"),
        0x32 => Some("inventory_slot"),
        0x34 => Some("crafting_data"),
        0x38 => Some("block_entity_data"),
        0x3a => Some("level_chunk"),
        0x3b => Some("set_commands_enabled"),
        0x3c => Some("set_difficulty"),
        0x3f => Some("player_list"),
        0x45 => Some("request_chunk_radius"),
        0x46 => Some("chunk_radius_update"),
        0x48 => Some("game_rules_changed"),
        0x4c => Some("available_commands"),
        0x4d => Some("command_request"),
        0x4f => Some("command_output"),
        0x55 => Some("transfer"),
        0x56 => Some("play_sound"),
        0x57 => Some("stop_sound"),
        0x58 => Some("set_title"),
        0x64 => Some("modal_form_request"),
        0x65 => Some("modal_form_response"),
        0x6a => Some("remove_objective"),
        0x6b => Some("set_display_objective"),
        0x6c => Some("set_score"),
        0x6f => Some("move_entity_delta"),
        0x70 => Some("set_scoreboard_identity"),
        0x71 => Some("set_local_player_as_initialized"),
        0x72 => Some("update_soft_enum"),
        0x73 => Some("network_stack_latency"),
        0x75 => Some("script_custom_event"),
        0x76 => Some("spawn_particle_effect"),
        0x77 => Some("available_entity_identifiers"),
        0x78 => Some("level_sound_event_v2"),
        0x79 => Some("network_chunk_publisher_update"),
        0x7a => Some("biome_definition_list"),
        0x7b => Some("level_sound_event"),
        0x81 => Some("client_cache_status"),
        0x8f => Some("network_settings"),
        0x90 => Some("player_auth_input"),
        0x91 => Some("creative_content"),
        0x92 => Some("player_enchant_options"),
        0x93 => Some("item_stack_request"),
        0x94 => Some("item_stack_response"),
        0xa1 => Some("correct_player_move_prediction"),
        0xa2 => Some("item_registry"),
        0xa5 => Some("sync_entity_property"),
        0xa0 => Some("player_fog"),
        0xbb => Some("update_abilities"),
        0xbc => Some("update_adventure_settings"),
        0xba => Some("toast_request"),
        0xc7 => Some("unlocked_recipes"),
        0xc6 => Some("camera_presets"),
        0x12e => Some("trim_data"),
        0x134 => Some("set_hud"),
        0x146 => Some("player_location"),
        _ => None,
    }
}

fn read_play_status(packet: &[u8], offset: usize) -> Option<i32> {
    let bytes: [u8; 4] = packet.get(offset..offset + 4)?.try_into().ok()?;
    let little = i32::from_le_bytes(bytes);
    if (0..=9).contains(&little) {
        return Some(little);
    }
    let big = i32::from_be_bytes(bytes);
    (0..=9).contains(&big).then_some(big)
}

fn read_network_stack_latency(packet: &[u8], offset: usize) -> Option<ObservedPacket> {
    let payload_len = packet.len().checked_sub(offset)?;
    if payload_len != 9 {
        eprintln!(
            "[BEDROCK_RX] id=115 name=network_stack_latency decode_failed=true payload_len={} expected_payload_len=9",
            payload_len
        );
        return None;
    }
    let timestamp = u64::from_le_bytes(packet.get(offset..offset + 8)?.try_into().ok()?);
    let needs_response = *packet.get(offset + 8)? != 0;
    if should_log_limited(
        &NETWORK_STACK_LATENCY_RX_LOGS,
        NETWORK_STACK_LATENCY_DEFAULT_LOG_LIMIT,
    ) {
        eprintln!(
            "[BEDROCK_RX] id=115 name=network_stack_latency timestamp={} needs_response={}",
            timestamp, needs_response
        );
    }
    Some(ObservedPacket::NetworkStackLatency {
        timestamp,
        needs_response,
    })
}

fn read_disconnect(packet: &[u8], payload_offset: usize) -> Option<ObservedDisconnect> {
    let mut offset = payload_offset;
    let reason = read_trace_zigzag_i32(packet, &mut offset)?;
    let hide_reason = *packet.get(offset)? != 0;
    offset += 1;
    let (message, filtered_message) = if hide_reason {
        (None, None)
    } else {
        (
            read_trace_string(packet, &mut offset),
            read_trace_string(packet, &mut offset),
        )
    };
    eprintln!(
        "[BEDROCK_DISCONNECT] reason={} hide_reason={} message={} filtered_message={}",
        reason,
        hide_reason,
        message.as_deref().unwrap_or(""),
        filtered_message.as_deref().unwrap_or("")
    );
    Some(ObservedDisconnect {
        reason,
        hide_reason,
        message,
        filtered_message,
    })
}

fn read_text_observation(packet: &[u8], payload_offset: usize) -> Option<ObservedText> {
    let payload = packet.get(payload_offset..)?;
    let mut strings = Vec::new();
    for start in 0..payload.len() {
        let mut offset = start;
        let Some(len) = read_trace_var_u32(payload, &mut offset).map(|len| len as usize) else {
            continue;
        };
        if len == 0 || len > 512 {
            continue;
        }
        let end = offset.saturating_add(len);
        let Some(bytes) = payload.get(offset..end) else {
            continue;
        };
        let Ok(value) = std::str::from_utf8(bytes) else {
            continue;
        };
        if value
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
            && value.chars().any(|ch| !ch.is_whitespace())
            && !strings.iter().any(|seen| seen == value)
        {
            strings.push(value.to_string());
        }
    }
    if trace_packets_enabled() && !strings.is_empty() {
        eprintln!("[CHAT_RX_RAW] strings={:?}", strings);
    }
    Some(ObservedText { strings })
}

fn read_inventory_content(
    packet: &[u8],
    payload_offset: usize,
) -> Option<Vec<ObservedInventoryItem>> {
    let mut offset = payload_offset;
    let window_id = read_trace_var_u32(packet, &mut offset)?;
    let count = read_trace_var_u32(packet, &mut offset)?;
    if count > 512 {
        eprintln!(
            "[GAMEPLAY_INVENTORY_RAW] packet=InventoryContent decode_skipped=true reason=unreasonable_count count={}",
            count
        );
        return None;
    }

    let mut items = Vec::new();
    for slot in 0..count {
        let item = read_inventory_item(packet, &mut offset, window_id, slot)?;
        if item.item_id != 0 {
            items.push(item);
        }
    }
    let container_name = read_full_container_name(packet, &mut offset)?;
    for item in &mut items {
        item.container_type = Some(container_name.container_type);
        item.dynamic_container_id = container_name.dynamic_id;
        eprintln!(
            "[GAMEPLAY_INVENTORY_RAW] packet=InventoryContent container={} slot={} item_id={} stack_id={} full_container_type={} dynamic_id={} item_len={}",
            item.container_id,
            item.slot,
            item.item_id,
            item.stack_id
                .map(|stack_id| stack_id.to_string())
                .unwrap_or_else(|| "none".to_string()),
            container_name.container_type,
            container_name
                .dynamic_id
                .map(|dynamic_id| dynamic_id.to_string())
                .unwrap_or_else(|| "none".to_string()),
            item.item_bytes.len()
        );
    }
    skip_inventory_item(packet, &mut offset)?;
    eprintln!(
        "[GAMEPLAY_INVENTORY_RAW] packet=InventoryContent container={} count={} non_air={} offset={} remaining={}",
        window_id,
        count,
        items.len(),
        offset,
        packet.len().saturating_sub(offset)
    );
    Some(items)
}

fn read_inventory_slot(
    packet: &[u8],
    payload_offset: usize,
) -> Option<Option<ObservedInventoryItem>> {
    let mut offset = payload_offset;
    let window_id = read_trace_var_u32(packet, &mut offset)?;
    let slot = read_trace_var_u32(packet, &mut offset)?;
    let container_name = read_full_container_name(packet, &mut offset)?;
    skip_inventory_item(packet, &mut offset)?;
    let mut item = read_inventory_item(packet, &mut offset, window_id, slot)?;
    item.container_type = Some(container_name.container_type);
    item.dynamic_container_id = container_name.dynamic_id;
    if item.item_id != 0 {
        eprintln!(
            "[GAMEPLAY_INVENTORY_RAW] packet=InventorySlot container={} slot={} item_id={} stack_id={} full_container_type={} dynamic_id={} item_len={}",
            item.container_id,
            item.slot,
            item.item_id,
            item.stack_id
                .map(|stack_id| stack_id.to_string())
                .unwrap_or_else(|| "none".to_string()),
            container_name.container_type,
            container_name
                .dynamic_id
                .map(|dynamic_id| dynamic_id.to_string())
                .unwrap_or_else(|| "none".to_string()),
            item.item_bytes.len()
        );
        Some(Some(item))
    } else {
        Some(None)
    }
}

fn read_item_stack_response(
    packet: &[u8],
    payload_offset: usize,
) -> Option<Vec<ObservedItemStackResponse>> {
    let mut offset = payload_offset;
    let count = read_trace_var_u32(packet, &mut offset)? as usize;
    let mut responses = Vec::with_capacity(count);

    for _ in 0..count {
        let result = *packet.get(offset)?;
        offset += 1;
        let raw_client_request_id = read_trace_var_u32(packet, &mut offset)?;
        let client_request_id =
            ((raw_client_request_id >> 1) as i32) ^ (-((raw_client_request_id & 1) as i32));
        let trailing_bytes = if result == 0 {
            packet.len().saturating_sub(offset)
        } else {
            0
        };
        responses.push(ObservedItemStackResponse {
            result,
            result_name: item_stack_response_result_name(result),
            raw_client_request_id,
            client_request_id,
            trailing_bytes,
        });
        if result == 0 {
            break;
        }
    }

    Some(responses)
}

fn item_stack_response_result_name(result: u8) -> &'static str {
    match result {
        0 => "Success",
        1 => "Error",
        2 => "InvalidRequestActionType",
        3 => "ActionRequestNotAllowed",
        4 => "ScreenHandlerEndRequestFailed",
        5 => "ItemRequestActionHandlerCommitFailed",
        6 => "InvalidRequestCraftActionType",
        7 => "InvalidCraftRequest",
        8 => "InvalidCraftRequestScreen",
        9 => "InvalidCraftResult",
        10 => "InvalidCraftResultIndex",
        11 => "InvalidCraftResultItem",
        12 => "InvalidItemNetId",
        13 => "MissingCreatedOutputContainer",
        14 => "FailedToSetCreatedItemOutputSlot",
        15 => "RequestAlreadyInProgress",
        16 => "FailedToInitSparseContainer",
        17 => "ResultTransferFailed",
        18 => "ExpectedItemSlotNotFullyConsumed",
        19 => "ExpectedAnywhereItemNotFullyConsumed",
        20 => "ItemAlreadyConsumedFromSlot",
        21 => "ConsumedTooMuchFromSlot",
        22 => "MismatchSlotExpectedConsumedItem",
        23 => "MismatchSlotExpectedConsumedItemNetIdVariant",
        24 => "FailedToMatchExpectedSlotConsumedItem",
        25 => "FailedToMatchExpectedAllowedAnywhereConsumedItem",
        26 => "ConsumedItemOutOfAllowedSlotRange",
        27 => "ConsumedItemNotAllowed",
        28 => "PlayerNotInCreativeMode",
        29 => "InvalidExperimentalRecipeRequest",
        30 => "FailedToCraftCreative",
        31 => "FailedToGetLevelRecipe",
        32 => "FailedToFindRecipeByNetId",
        33 => "MismatchedCraftingSize",
        34 => "MissingInputSparseContainer",
        35 => "MismatchedRecipeForInputGridItems",
        36 => "EmptyCraftResults",
        37 => "FailedToEnchant",
        38 => "MissingInputItem",
        39 => "InsufficientPlayerLevelToEnchant",
        40 => "MissingMaterialItem",
        41 => "MissingActor",
        42 => "UnknownPrimaryEffect",
        43 => "PrimaryEffectOutOfRange",
        44 => "PrimaryEffectUnavailable",
        45 => "SecondaryEffectOutOfRange",
        46 => "SecondaryEffectUnavailable",
        47 => "DstContainerEqualToCreatedOutputContainer",
        48 => "DstContainerAndSlotEqualToSrcContainerAndSlot",
        49 => "FailedToValidateSrcSlot",
        50 => "FailedToValidateDstSlot",
        51 => "InvalidAdjustedAmount",
        52 => "InvalidItemSetType",
        53 => "InvalidTransferAmount",
        54 => "CannotSwapItem",
        55 => "CannotPlaceItem",
        56 => "UnhandledItemSetType",
        57 => "InvalidRemovedAmount",
        58 => "InvalidRegion",
        59 => "CannotDropItem",
        60 => "CannotDestroyItem",
        61 => "InvalidSourceContainer",
        62 => "ItemNotConsumed",
        63 => "InvalidNumCrafts",
        64 => "InvalidCraftResultStackSize",
        65 => "CannotRemoveItem",
        66 => "CannotConsumeItem",
        67 => "ScreenStackError",
        _ => "Unknown",
    }
}

#[derive(Debug, Clone, Copy)]
struct ObservedFullContainerName {
    container_type: u8,
    dynamic_id: Option<u32>,
}

fn read_full_container_name(
    packet: &[u8],
    offset: &mut usize,
) -> Option<ObservedFullContainerName> {
    let container_type = *packet.get(*offset)?;
    *offset = (*offset).checked_add(1)?;
    let dynamic_id = if read_trace_bool(packet, offset).ok()? {
        let bytes: [u8; 4] = packet.get(*offset..*offset + 4)?.try_into().ok()?;
        *offset += 4;
        Some(u32::from_le_bytes(bytes))
    } else {
        None
    };
    (*offset <= packet.len()).then_some(ObservedFullContainerName {
        container_type,
        dynamic_id,
    })
}

fn read_inventory_item(
    packet: &[u8],
    offset: &mut usize,
    container_id: u32,
    slot: u32,
) -> Option<ObservedInventoryItem> {
    let start = *offset;
    let item = read_inventory_item_header(packet, offset)?;
    let item_bytes = packet.get(start..*offset)?.to_vec();
    Some(ObservedInventoryItem {
        container_id,
        slot,
        item_id: item.item_id,
        stack_id: item.stack_id,
        container_type: None,
        dynamic_container_id: None,
        item_bytes,
    })
}

fn skip_inventory_item(packet: &[u8], offset: &mut usize) -> Option<i32> {
    read_inventory_item_header(packet, offset).map(|item| item.item_id)
}

#[derive(Debug, Clone, Copy)]
struct ObservedInventoryItemHeader {
    item_id: i32,
    stack_id: Option<i32>,
}

fn read_inventory_item_header(
    packet: &[u8],
    offset: &mut usize,
) -> Option<ObservedInventoryItemHeader> {
    let item_id = read_trace_zigzag_i32(packet, offset)?;
    if item_id == 0 {
        return Some(ObservedInventoryItemHeader {
            item_id,
            stack_id: None,
        });
    }
    skip_trace_bytes(packet, offset, 2).ok()?;
    read_trace_var_u32(packet, offset)?;
    let has_stack_id = *packet.get(*offset)?;
    *offset += 1;
    let stack_id = if has_stack_id != 0 {
        Some(read_trace_zigzag_i32(packet, offset)?)
    } else {
        None
    };
    read_trace_zigzag_i32(packet, offset)?;
    let extra_len = read_trace_var_u32(packet, offset)? as usize;
    skip_trace_bytes(packet, offset, extra_len).ok()?;
    Some(ObservedInventoryItemHeader { item_id, stack_id })
}

fn read_add_item_entity(packet: &[u8], payload_offset: usize) -> Option<ObservedPacket> {
    let mut offset = payload_offset;
    let entity_id = read_trace_zigzag_i64(packet, &mut offset)?;
    let runtime_entity_id = read_trace_var_u64(packet, &mut offset)?;
    let item_start = offset;
    let item = read_inventory_item_header(packet, &mut offset)?;
    let item_bytes = packet.get(item_start..offset)?.to_vec();
    let position = read_trace_vec3f(packet, &mut offset).ok()?;
    let velocity = read_trace_vec3f(packet, &mut offset).ok()?;
    eprintln!(
        "[GAMEPLAY_ITEM_ENTITY_RX] packet=AddItemEntity entity_id={} runtime_id={} item_id={} stack_id={} position={} velocity={} item_len={} offset={} remaining={}",
        entity_id,
        runtime_entity_id,
        item.item_id,
        item.stack_id
            .map(|stack_id| stack_id.to_string())
            .unwrap_or_else(|| "none".to_string()),
        format_trace_position(position),
        format_trace_position(velocity),
        item_bytes.len(),
        offset,
        packet.len().saturating_sub(offset)
    );
    Some(ObservedPacket::AddItemEntity(ObservedItemEntity {
        entity_id,
        runtime_entity_id,
        item_id: item.item_id,
        stack_id: item.stack_id,
        position,
        velocity,
        item_bytes,
    }))
}

fn read_take_item_entity(packet: &[u8], payload_offset: usize) -> Option<ObservedPacket> {
    let mut offset = payload_offset;
    let runtime_entity_id = read_trace_var_u64(packet, &mut offset)?;
    let target_runtime_entity_id = read_trace_var_u32(packet, &mut offset)?;
    eprintln!(
        "[GAMEPLAY_ITEM_ENTITY_RX] packet=TakeItemEntity runtime_id={} target_runtime_id={} offset={} remaining={}",
        runtime_entity_id,
        target_runtime_entity_id,
        offset,
        packet.len().saturating_sub(offset)
    );
    Some(ObservedPacket::TakeItemEntity {
        runtime_entity_id,
        target_runtime_entity_id,
    })
}

const CHUNK_AIR_RUNTIME_ID: u32 = 12530;
const MAX_LEVEL_CHUNK_SAMPLES: usize = 256;
const SUBCHUNK_SAMPLE_COORDS: [usize; 4] = [1, 5, 9, 13];

struct LevelChunkSamples {
    samples: Vec<ObservedBlockSample>,
    decoded_subchunks: u32,
    stop_reason: Option<String>,
}

fn read_network_chunk_publisher_update(
    packet: &[u8],
    payload_offset: usize,
) -> Option<ObservedPacket> {
    let mut offset = payload_offset;
    let x = read_trace_zigzag_i32(packet, &mut offset)?;
    let y = read_trace_zigzag_i32(packet, &mut offset)?;
    let z = read_trace_zigzag_i32(packet, &mut offset)?;
    let radius = read_trace_var_u32(packet, &mut offset)?;
    if packet.len().saturating_sub(offset) >= 4 {
        let saved_count = read_le_u32(packet, &mut offset)? as usize;
        let bytes = saved_count.checked_mul(8)?;
        skip_trace_bytes(packet, &mut offset, bytes).ok()?;
    }
    if trace_chunks_enabled() {
        eprintln!(
            "[GAMEPLAY_CHUNK_PUBLISHER] position={},{},{} radius={} offset={} remaining={}",
            x,
            y,
            z,
            radius,
            offset,
            packet.len().saturating_sub(offset)
        );
    }
    Some(ObservedPacket::NetworkChunkPublisherUpdate { x, y, z, radius })
}

fn read_level_chunk_observation(packet: &[u8], payload_offset: usize) -> Option<ObservedPacket> {
    let mut offset = payload_offset;
    let chunk_x = read_trace_zigzag_i32(packet, &mut offset)?;
    let chunk_z = read_trace_zigzag_i32(packet, &mut offset)?;
    let dimension = read_trace_zigzag_i32(packet, &mut offset)?;
    let sub_chunk_count = read_trace_var_i32(packet, &mut offset)?;
    if sub_chunk_count == -2 {
        skip_trace_bytes(packet, &mut offset, 2).ok()?;
    }
    let cache_enabled = read_trace_bool(packet, &mut offset).ok()?;
    if cache_enabled {
        let blob_count = read_trace_var_u32(packet, &mut offset)? as usize;
        skip_trace_bytes(packet, &mut offset, blob_count.checked_mul(8)?).ok()?;
    }
    let payload_len = read_trace_var_u32(packet, &mut offset)? as usize;
    let payload_end = offset.checked_add(payload_len)?;
    let payload = packet.get(offset..payload_end)?;
    let samples = if cache_enabled || sub_chunk_count < 0 {
        Vec::new()
    } else {
        match sample_level_chunk_payload(chunk_x, chunk_z, sub_chunk_count as u32, payload) {
            Ok(result) => {
                if let Some(reason) = result.stop_reason.filter(|_| trace_chunks_enabled()) {
                    eprintln!(
                        "[GAMEPLAY_CHUNK_SAMPLE_STOP] chunk={},{} dimension={} payload_len={} decoded_subchunks={} samples={} reason={}",
                        chunk_x,
                        chunk_z,
                        dimension,
                        payload_len,
                        result.decoded_subchunks,
                        result.samples.len(),
                        reason
                    );
                }
                result.samples
            }
            Err(error) => {
                if trace_chunks_enabled() {
                    eprintln!(
                        "[GAMEPLAY_CHUNK_SAMPLE_ERROR] chunk={},{} dimension={} payload_len={} error={}",
                        chunk_x, chunk_z, dimension, payload_len, error
                    );
                }
                Vec::new()
            }
        }
    };
    if trace_chunks_enabled() {
        eprintln!(
            "[GAMEPLAY_CHUNK] chunk={},{} dimension={} sub_chunks={} cache_enabled={} payload_len={} samples={}",
            chunk_x,
            chunk_z,
            dimension,
            sub_chunk_count,
            cache_enabled,
            payload_len,
            samples.len()
        );
    }
    Some(ObservedPacket::LevelChunk {
        chunk_x,
        chunk_z,
        dimension,
        samples,
    })
}

fn sample_level_chunk_payload(
    chunk_x: i32,
    chunk_z: i32,
    sub_chunk_count: u32,
    payload: &[u8],
) -> Result<LevelChunkSamples, String> {
    let mut offset = 0usize;
    let mut samples = Vec::new();
    let mut decoded_subchunks = 0u32;
    let sub_chunks = sub_chunk_count.min(32);
    let mut sub_chunk_index = 0u32;
    let mut last_subchunk_y: Option<i32> = None;
    while sub_chunk_index < sub_chunks {
        if offset >= payload.len() || samples.len() >= MAX_LEVEL_CHUNK_SAMPLES {
            break;
        }
        let version =
            read_u8(payload, &mut offset).ok_or_else(|| "missing subchunk version".to_string())?;
        let layer_count = match version {
            1 => 1,
            8 | 9 => read_u8(payload, &mut offset)
                .ok_or_else(|| "missing subchunk layer count".to_string())?,
            0 => break,
            other => {
                let unsupported_offset = offset.saturating_sub(1);
                let context_start = unsupported_offset.saturating_sub(16);
                let context_end = payload.len().min(unsupported_offset.saturating_add(64));
                if let Some(recovered_offset) = find_previous_subchunk_start(
                    payload,
                    unsupported_offset,
                    last_subchunk_y.map(|y| y.saturating_add(1)),
                ) {
                    if trace_chunks_enabled() {
                        eprintln!(
                            "[GAMEPLAY_CHUNK_SAMPLE_RECOVER] unsupported_version={} unsupported_offset={} recovered_offset={} expected_y={:?}",
                            other,
                            unsupported_offset,
                            recovered_offset,
                            last_subchunk_y.map(|y| y.saturating_add(1))
                        );
                    }
                    offset = recovered_offset;
                    continue;
                }
                let reason = format!(
                    "unsupported subchunk version {other} at payload_offset={} context_hex={}",
                    unsupported_offset,
                    hex_dump(&payload[context_start..context_end])
                );
                if decoded_subchunks > 0 || !samples.is_empty() {
                    return Ok(LevelChunkSamples {
                        samples,
                        decoded_subchunks,
                        stop_reason: Some(reason),
                    });
                }
                return Err(reason);
            }
        };
        let subchunk_y = if version == 9 {
            read_i8(payload, &mut offset).ok_or_else(|| "missing subchunk y index".to_string())?
                as i32
        } else {
            sub_chunk_index as i32
        };
        last_subchunk_y = Some(subchunk_y);
        for layer in 0..layer_count {
            let collect = layer == 0 && samples.len() < MAX_LEVEL_CHUNK_SAMPLES;
            let layer_result = read_subchunk_runtime_layer(
                payload,
                &mut offset,
                chunk_x,
                chunk_z,
                subchunk_y,
                collect,
            );
            let mut layer_samples = match layer_result {
                Ok(layer_samples) => layer_samples,
                Err(error) if decoded_subchunks > 0 || !samples.is_empty() => {
                    return Ok(LevelChunkSamples {
                        samples,
                        decoded_subchunks,
                        stop_reason: Some(format!(
                            "layer decode stopped at subchunk={} layer={} offset={} error={} context_hex={}",
                            sub_chunk_index,
                            layer,
                            offset,
                            error,
                            hex_dump(
                                &payload[offset.saturating_sub(16)
                                    ..payload.len().min(offset.saturating_add(64))]
                            )
                        )),
                    });
                }
                Err(error) => return Err(error),
            };
            samples.append(&mut layer_samples);
            if samples.len() >= MAX_LEVEL_CHUNK_SAMPLES {
                samples.truncate(MAX_LEVEL_CHUNK_SAMPLES);
            }
        }
        decoded_subchunks = decoded_subchunks.saturating_add(1);
        sub_chunk_index = sub_chunk_index.saturating_add(1);
    }
    Ok(LevelChunkSamples {
        samples,
        decoded_subchunks,
        stop_reason: None,
    })
}

fn find_previous_subchunk_start(
    payload: &[u8],
    unsupported_offset: usize,
    expected_y: Option<i32>,
) -> Option<usize> {
    let start = unsupported_offset.saturating_sub(24);
    (start..unsupported_offset)
        .rev()
        .find(|candidate| is_plausible_version_9_subchunk_start(payload, *candidate, expected_y))
}

fn is_plausible_version_9_subchunk_start(
    payload: &[u8],
    offset: usize,
    expected_y: Option<i32>,
) -> bool {
    if payload.get(offset).copied() != Some(9) {
        return false;
    }
    let Some(layer_count) = payload.get(offset + 1).copied() else {
        return false;
    };
    if layer_count > 4 {
        return false;
    }
    let Some(subchunk_y) = payload
        .get(offset + 2)
        .copied()
        .map(|value| value as i8 as i32)
    else {
        return false;
    };
    if let Some(expected_y) = expected_y {
        if subchunk_y != expected_y {
            return false;
        }
    }
    if layer_count == 0 {
        return true;
    }
    let Some(header) = payload.get(offset + 3).copied() else {
        return false;
    };
    let bits_per_index = header >> 1;
    let runtime_palette = header & 1 == 1;
    runtime_palette && palette_word_count(bits_per_index).is_some()
}

fn read_subchunk_runtime_layer(
    payload: &[u8],
    offset: &mut usize,
    chunk_x: i32,
    chunk_z: i32,
    subchunk_y: i32,
    collect_samples: bool,
) -> Result<Vec<ObservedBlockSample>, String> {
    let header = read_u8(payload, offset).ok_or_else(|| "missing palette header".to_string())?;
    let bits_per_index = header >> 1;
    let runtime_palette = header & 1 == 1;
    let word_count = palette_word_count(bits_per_index)
        .ok_or_else(|| format!("unsupported bits_per_index {bits_per_index}"))?;
    let mut words = Vec::with_capacity(word_count);
    for _ in 0..word_count {
        words.push(
            read_le_u32(payload, offset).ok_or_else(|| "truncated palette words".to_string())?,
        );
    }
    if !runtime_palette {
        return Err("persistent NBT subchunk palettes are not sampled yet".to_string());
    }
    let palette = if bits_per_index == 0 {
        let runtime_id = read_trace_var_i32(payload, offset)
            .ok_or_else(|| "missing singleton runtime palette entry".to_string())?;
        vec![runtime_id as u32]
    } else {
        let palette_len = read_trace_var_i32(payload, offset)
            .ok_or_else(|| "missing runtime palette length".to_string())?;
        if palette_len == 0 || palette_len > 4096 {
            return Err(format!("invalid runtime palette length {palette_len}"));
        }
        let palette_len = palette_len as usize;
        let mut palette = Vec::with_capacity(palette_len);
        for _ in 0..palette_len {
            let runtime_id = read_trace_var_i32(payload, offset)
                .ok_or_else(|| "truncated runtime palette entry".to_string())?;
            palette.push(runtime_id as u32);
        }
        palette
    };
    if !collect_samples {
        return Ok(Vec::new());
    }

    let mut samples = Vec::new();
    for local_x in SUBCHUNK_SAMPLE_COORDS {
        for local_z in SUBCHUNK_SAMPLE_COORDS {
            for local_y in (0..16usize).rev() {
                let block_index = local_y + local_z * 16 + local_x * 256;
                let palette_index = palette_index_at(&words, bits_per_index, block_index)? as usize;
                let Some(runtime_id) = palette.get(palette_index).copied() else {
                    continue;
                };
                if runtime_id == 0 || runtime_id == CHUNK_AIR_RUNTIME_ID {
                    continue;
                }
                samples.push(ObservedBlockSample {
                    x: chunk_x.saturating_mul(16).saturating_add(local_x as i32),
                    y: subchunk_y.saturating_mul(16).saturating_add(local_y as i32),
                    z: chunk_z.saturating_mul(16).saturating_add(local_z as i32),
                    runtime_id,
                });
                break;
            }
        }
    }
    Ok(samples)
}

fn palette_word_count(bits_per_index: u8) -> Option<usize> {
    if bits_per_index == 0 {
        return Some(0);
    }
    if !matches!(bits_per_index, 1 | 2 | 3 | 4 | 5 | 6 | 8 | 16) {
        return None;
    }
    let indices_per_word = 32usize / bits_per_index as usize;
    Some((4096 + indices_per_word - 1) / indices_per_word)
}

fn palette_index_at(words: &[u32], bits_per_index: u8, block_index: usize) -> Result<u32, String> {
    if bits_per_index == 0 {
        return Ok(0);
    }
    let indices_per_word = 32usize / bits_per_index as usize;
    let word_index = block_index / indices_per_word;
    let bit_index = (block_index % indices_per_word) * bits_per_index as usize;
    let word = *words
        .get(word_index)
        .ok_or_else(|| "palette index outside word array".to_string())?;
    let mask = (1u32 << bits_per_index) - 1;
    Ok((word >> bit_index) & mask)
}

fn read_u8(bytes: &[u8], offset: &mut usize) -> Option<u8> {
    let value = *bytes.get(*offset)?;
    *offset += 1;
    Some(value)
}

fn read_i8(bytes: &[u8], offset: &mut usize) -> Option<i8> {
    read_u8(bytes, offset).map(|value| value as i8)
}

fn read_le_u32(bytes: &[u8], offset: &mut usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let value = u32::from_le_bytes(bytes.get(*offset..end)?.try_into().ok()?);
    *offset = end;
    Some(value)
}

fn read_block_coordinates(packet: &[u8], offset: &mut usize) -> Option<(i32, i32, i32)> {
    let x = read_trace_zigzag_i32(packet, offset)?;
    let y = read_trace_var_i32(packet, offset)?;
    let z = read_trace_zigzag_i32(packet, offset)?;
    Some((x, y, z))
}

fn read_update_block(packet: &[u8], payload_offset: usize) -> Option<ObservedPacket> {
    let mut offset = payload_offset;
    let (x, y, z) = read_block_coordinates(packet, &mut offset)?;
    let runtime_id = read_trace_var_u32(packet, &mut offset)?;
    let flags = read_trace_var_u32(packet, &mut offset)?;
    let layer = read_trace_var_u32(packet, &mut offset)?;
    if trace_chunks_enabled() {
        eprintln!(
            "[GAMEPLAY_RX_RAW] packet=UpdateBlock target={},{},{} runtime_id={} flags={} layer={}",
            x, y, z, runtime_id, flags, layer
        );
    }
    Some(ObservedPacket::UpdateBlock {
        x,
        y,
        z,
        runtime_id,
        flags,
        layer,
    })
}

fn merge_locally_observed_with_raw_keepalives(
    raw_observed: Vec<ObservedPacket>,
    mut locally_observed: Vec<ObservedPacket>,
) -> Vec<ObservedPacket> {
    locally_observed.extend(
        raw_observed
            .into_iter()
            .filter(|packet| matches!(packet, ObservedPacket::NetworkStackLatency { .. })),
    );
    locally_observed
}

fn encode_network_stack_latency_payload(timestamp: u64, needs_response: bool) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9);
    payload.extend_from_slice(&timestamp.to_le_bytes());
    payload.push(u8::from(needs_response));
    payload
}

fn encode_network_stack_latency_packet_stream(timestamp: u64, needs_response: bool) -> Vec<u8> {
    let mut packet = Vec::with_capacity(10);
    write_unsigned_varint_u32(0x73, &mut packet);
    packet.extend_from_slice(&encode_network_stack_latency_payload(
        timestamp,
        needs_response,
    ));

    let mut stream = Vec::with_capacity(packet.len() + 2);
    write_unsigned_varint_u32(packet.len() as u32, &mut stream);
    stream.extend_from_slice(&packet);
    stream
}

fn network_stack_latency_response_timestamp(timestamp: u64) -> u64 {
    scale_latency_timestamp_for_donut(timestamp)
}

fn scale_latency_timestamp_for_donut(timestamp: u64) -> u64 {
    timestamp.wrapping_mul(NETWORK_STACK_LATENCY_MAGNITUDE)
}

fn read_var_u32_from(packet: &[u8], offset: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    let mut shift = 0u32;

    for _ in 0..5 {
        let byte = *packet.get(*offset)?;
        *offset += 1;
        value |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }

    None
}

fn login_server_address(host: &str, port: u16) -> String {
    if let Ok(address) = std::env::var("BEDROCK_LOGIN_SERVER_ADDRESS") {
        let address = address.trim();
        if !address.is_empty() {
            return address.to_string();
        }
    }

    format!("{host}:{port}")
}

fn login_token_for_session(session: &ProvisionedBedrockSession) -> &str {
    let source = std::env::var("BEDROCK_LOGIN_TOKEN_SOURCE").unwrap_or_default();
    if source.eq_ignore_ascii_case("legacy") {
        return &session.legacy_bedrock_token;
    }

    &session.bedrock_login_token
}

fn override_connection_request() -> EngineResult<Option<Vec<u8>>> {
    let value = if let Ok(path) = std::env::var("BEDROCK_LOGIN_CONNECTION_REQUEST_B64_FILE") {
        let path = path.trim();
        if path.is_empty() {
            return Ok(None);
        }
        std::fs::read_to_string(path).map_err(|err| {
            EngineError::Bedrock(format!("read override Login request file {path}: {err}"))
        })?
    } else if let Ok(value) = std::env::var("BEDROCK_LOGIN_CONNECTION_REQUEST_B64") {
        value
    } else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let bytes = STANDARD
        .decode(value)
        .map_err(|err| EngineError::Bedrock(format!("decode override Login request: {err}")))?;
    eprintln!(
        "[LOGIN] using BEDROCK_LOGIN_CONNECTION_REQUEST_B64 override len={}",
        bytes.len()
    );
    Ok(Some(bytes))
}

fn read_container_open_observed(packet: &[u8], payload_offset: usize) -> Option<ObservedPacket> {
    let mut offset = payload_offset;
    let container_id = *packet.get(offset)? as i8;
    offset += 1;
    let container_type = *packet.get(offset)? as i8;
    offset += 1;
    // block position: zigzag_i32, var_u32, zigzag_i32
    let x = read_trace_zigzag_i32(packet, &mut offset)?;
    let y = read_trace_var_u32(packet, &mut offset)?;
    let z = read_trace_zigzag_i32(packet, &mut offset)?;
    // entity_unique_id: zigzag_i64
    let entity_unique_id = read_trace_zigzag_i64(packet, &mut offset)?;

    eprintln!(
        "[BEDROCK_RX] id=46 name=container_open container_id={} container_type={} pos={},{},{} entity_id={}",
        container_id, container_type, x, y, z, entity_unique_id
    );

    Some(ObservedPacket::ContainerOpen {
        container_id: container_id as u8,
        container_type: container_type as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn request_network_settings_framed_encoding_is_exact() {
        let packet = BedrockProto::RequestNetworkSettings(RequestNetworkSettingsPacket {
            protocol_version: 898,
        });
        let payload = codec::encode_packets(&[packet], None, None, ProtoVersion::V898).unwrap();

        assert_eq!(payload.len(), 7);
        // 898 = 0x00000382, encoded as big-endian u32 -> [0x00, 0x00, 0x03, 0x82]
        assert_eq!(payload, vec![0x06, 0xc1, 0x01, 0x00, 0x00, 0x03, 0x82]);
    }

    #[test]
    fn protocol_options_resolve_priority_and_codec() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV);
        env::remove_var(LEGACY_BEDROCK_PROTOCOL_VERSION_ENV);

        // Test: default fallback
        let resolved = BedrockProtocolOptions::resolve(None).unwrap();
        assert_eq!(
            resolved.requested_protocol_version,
            DEFAULT_BEDROCK_PROTOCOL_VERSION
        );
        assert_eq!(resolved.codec_protocol_version, ProtoVersion::V898);
        assert_eq!(resolved.source, BedrockProtocolVersionSource::Default);
        assert!(resolved.codec_exact_match());

        // Test: legacy env var fallback
        env::set_var(LEGACY_BEDROCK_PROTOCOL_VERSION_ENV, "975");
        let resolved = BedrockProtocolOptions::resolve(None).unwrap();
        assert_eq!(resolved.requested_protocol_version, 975);
        assert_eq!(resolved.codec_protocol_version, ProtoVersion::V975);
        assert_eq!(resolved.source, BedrockProtocolVersionSource::LegacyEnv);

        // Test: TORCHFLOWER_BEDROCK_PROTOCOL_VERSION beats BEDROCK_PROTOCOL_VERSION
        env::set_var(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV, "766");
        let resolved = BedrockProtocolOptions::resolve(None).unwrap();
        assert_eq!(resolved.requested_protocol_version, 766);
        assert_eq!(resolved.codec_protocol_version, ProtoVersion::V766);
        assert_eq!(
            resolved.source,
            BedrockProtocolVersionSource::TorchflowerEnv
        );

        // Test: config protocol beats env protocol
        let resolved = BedrockProtocolOptions::resolve(Some(662)).unwrap();
        assert_eq!(resolved.requested_protocol_version, 662);
        assert_eq!(resolved.codec_protocol_version, ProtoVersion::V662);
        assert_eq!(resolved.source, BedrockProtocolVersionSource::Config);

        // Test: unknown unsupported protocol fallback to codec V898
        let resolved = BedrockProtocolOptions::from_config(899).unwrap();
        assert_eq!(resolved.requested_protocol_version, 899);
        assert_eq!(resolved.codec_protocol_version, ProtoVersion::V898);
        assert!(!resolved.codec_exact_match());

        // Test: invalid zero protocol version
        env::set_var(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV, "0");
        assert!(BedrockProtocolOptions::resolve(None).is_err());

        // Test: invalid negative protocol version
        env::set_var(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV, "-5");
        assert!(BedrockProtocolOptions::resolve(None).is_err());

        env::remove_var(TORCHFLOWER_BEDROCK_PROTOCOL_VERSION_ENV);
        env::remove_var(LEGACY_BEDROCK_PROTOCOL_VERSION_ENV);
    }

    #[test]
    fn early_disconnect_error_message_formatting() {
        let protocol_ver = 999;
        let codec_protocol_ver = 898;
        let reason = 2;
        let hide_reason = false;
        let message = Some("Banned".to_string());

        let err_msg = format!(
            "server did not return NetworkSettingsPacket; received early Disconnect before NetworkSettings. \
             protocol_version={}. codec_protocol_version={}. reason={}. hide_reason={}. message={:?}. \
             This usually means unsupported protocol version, invalid RequestNetworkSettings encoding/framing, or server rejected the client before login.",
            protocol_ver, codec_protocol_ver, reason, hide_reason, message
        );

        assert!(err_msg.contains("protocol_version=999"));
        assert!(err_msg.contains("codec_protocol_version=898"));
        assert!(err_msg.contains("reason=2"));
        assert!(err_msg.contains("hide_reason=false"));
        assert!(err_msg.contains("message=Some(\"Banned\")"));
    }

    #[test]
    fn packet_summary_reads_bedrock_batch_packets() {
        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x02, &[3, 0, 0, 0]);
        append_test_packet(&mut stream, 0x0b, &[0xaa, 0xbb, 0xcc]);

        let mut offset = 0usize;
        let first = read_packet_summary(&stream, &mut offset)
            .expect("first summary")
            .expect("first packet");
        assert_eq!(first.packet_id, 0x02);
        assert_eq!(first.payload_len, 4);
        assert_eq!(bedrock_packet_name(first.packet_id), "play_status");

        let second = read_packet_summary(&stream, &mut offset)
            .expect("second summary")
            .expect("second packet");
        assert_eq!(second.packet_id, 0x0b);
        assert_eq!(second.payload_len, 3);
        assert_eq!(bedrock_packet_name(second.packet_id), "start_game");

        assert!(read_packet_summary(&stream, &mut offset).unwrap().is_none());
    }

    #[test]
    fn registry_names_modern_command_and_sound_packets() {
        assert_eq!(bedrock_packet_name(0x4d), "command_request");
        assert_eq!(bedrock_packet_name(0x4f), "command_output");
        assert_eq!(bedrock_packet_name(0x56), "play_sound");
        assert_eq!(bedrock_packet_name(0x57), "stop_sound");
        assert_eq!(bedrock_packet_name(0xa0), "player_fog");
    }

    #[test]
    fn loose_text_candidates_extract_command_diagnostics() {
        let mut packet = Vec::new();
        write_unsigned_varint_u32(0x4f, &mut packet);
        let payload_offset = packet.len();
        packet.extend_from_slice(&[0xff, 0x00, 0x7f]);
        write_unsigned_varint_u32(3, &mut packet);
        packet.extend_from_slice(b"rtp");
        let message = b"Unknown command: rtp is not available here";
        write_unsigned_varint_u32(message.len() as u32, &mut packet);
        packet.extend_from_slice(message);
        packet.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x80]);

        let candidates = loose_text_candidates(&packet, payload_offset);
        assert!(candidates.iter().any(|candidate| candidate == "rtp"));
        assert!(candidates
            .iter()
            .any(|candidate| candidate == "Unknown command: rtp is not available here"));

        let prioritized = prioritize_command_text_candidates(&candidates);
        assert!(prioritized.iter().any(|candidate| candidate == "rtp"));
        assert!(prioritized
            .iter()
            .any(|candidate| candidate.contains("Unknown command")));
    }

    #[test]
    fn observe_packet_ids_uses_same_batch_parser() {
        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x02, &[3, 0, 0, 0]);
        append_test_packet(&mut stream, 0x73, &[1, 0, 0, 0, 0, 0, 0, 0, 1]);

        let observed = observe_packet_ids(&stream).expect("observed packets");
        assert!(matches!(observed[0], ObservedPacket::PlayStatus(Some(3))));
        assert!(matches!(
            observed[1],
            ObservedPacket::NetworkStackLatency {
                timestamp: 1,
                needs_response: true
            }
        ));
    }

    #[test]
    fn raw_start_game_observation_preserves_entity_and_runtime_ids() {
        let mut payload = Vec::new();
        write_test_zigzag_i64(-99, &mut payload);
        write_test_var_u64(1234, &mut payload);
        write_unsigned_varint_u32(1, &mut payload);
        payload.extend_from_slice(&10.0f32.to_le_bytes());
        payload.extend_from_slice(&64.0f32.to_le_bytes());
        payload.extend_from_slice(&20.0f32.to_le_bytes());
        payload.extend_from_slice(&90.0f32.to_le_bytes());
        payload.extend_from_slice(&15.0f32.to_le_bytes());

        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x0b, &payload);
        let observed = observe_packet_ids(&stream).expect("observed packets");
        let ObservedPacket::StartGame(Some(start_game)) = &observed[0] else {
            panic!("expected raw StartGame observation");
        };

        assert_eq!(start_game.entity_id, -99);
        assert_eq!(start_game.runtime_id, 1234);
        assert_eq!(start_game.position, (10.0, 64.0, 20.0));
        assert_eq!(start_game.rotation, (90.0, 15.0));
    }

    #[test]
    fn network_stack_latency_response_scales_small_timestamp_as_le_payload() {
        let incoming = 2_687_948u64;
        let response = network_stack_latency_response_timestamp(incoming);
        assert_eq!(response, 2_687_948_000_000);

        let payload = encode_network_stack_latency_payload(response, false);
        let mut expected = Vec::new();
        expected.extend_from_slice(&2_687_948_000_000u64.to_le_bytes());
        expected.push(0);
        assert_eq!(payload, expected);
    }

    #[test]
    fn network_stack_latency_response_scales_observed_valid_run_timestamp() {
        let incoming = 879_042u64;
        let response = network_stack_latency_response_timestamp(incoming);
        assert_eq!(response, 879_042_000_000);

        let payload = encode_network_stack_latency_payload(response, false);
        let mut expected = Vec::new();
        expected.extend_from_slice(&879_042_000_000u64.to_le_bytes());
        expected.push(0);
        assert_eq!(payload, expected);
    }

    #[test]
    fn network_stack_latency_response_wraps_large_timestamp_bits() {
        let incoming = 18_446_744_073_706_980_378u64;
        let response = network_stack_latency_response_timestamp(incoming);
        assert_eq!(response, incoming.wrapping_mul(1_000_000));

        let payload = encode_network_stack_latency_payload(response, false);
        let mut expected = Vec::new();
        expected.extend_from_slice(&incoming.wrapping_mul(1_000_000).to_le_bytes());
        expected.push(0);
        assert_eq!(payload, expected);
    }

    #[test]
    fn network_stack_latency_payload_body_is_exactly_nine_bytes() {
        let response = network_stack_latency_response_timestamp(2_687_948);
        let payload = encode_network_stack_latency_payload(response, false);
        assert_eq!(payload.len(), 9);
        assert_eq!(&payload[..8], &response.to_le_bytes());
        assert_eq!(payload[8], 0);
    }

    #[test]
    fn update_soft_enum_uses_varint_options_and_u8_action() {
        let packet = TorchFlowerUpdateSoftEnumPacket {
            enum_name: "commands".to_string(),
            options: vec!["spawn".to_string(), "home".to_string()],
            action_type: TorchFlowerSoftEnumAction::Update,
        };
        let mut payload = Vec::new();
        packet.serialize(&mut payload);

        let mut expected = Vec::new();
        expected.extend_from_slice(&[8]);
        expected.extend_from_slice(b"commands");
        expected.extend_from_slice(&[2]);
        expected.extend_from_slice(&[5]);
        expected.extend_from_slice(b"spawn");
        expected.extend_from_slice(&[4]);
        expected.extend_from_slice(b"home");
        expected.push(2);
        assert_eq!(payload, expected);

        let mut bedrock_packet = Vec::new();
        write_unsigned_varint_u32(0x72, &mut bedrock_packet);
        let payload_offset = bedrock_packet.len();
        bedrock_packet.extend_from_slice(&payload);

        let decoded =
            TorchFlowerUpdateSoftEnumPacket::deserialize(&bedrock_packet, payload_offset).unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn update_soft_enum_filter_removes_packet_before_upstream_decode() {
        let packet = TorchFlowerUpdateSoftEnumPacket {
            enum_name: "commands".to_string(),
            options: vec!["spawn".to_string()],
            action_type: TorchFlowerSoftEnumAction::Add,
        };
        let mut soft_enum_payload = Vec::new();
        packet.serialize(&mut soft_enum_payload);

        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x02, &[3, 0, 0, 0]);
        append_test_packet(&mut stream, 0x72, &soft_enum_payload);
        append_test_packet(&mut stream, 0x09, &[0]);

        let (filtered, local_observed) = filter_locally_decoded_packets(&stream).unwrap();
        assert_eq!(local_observed.len(), 1);
        assert!(matches!(local_observed[0], ObservedPacket::UpdateSoftEnum));
        let observed = observe_packet_ids(&filtered).unwrap();
        assert_eq!(observed.len(), 2);
        assert!(matches!(observed[0], ObservedPacket::PlayStatus(Some(3))));
        assert!(matches!(observed[1], ObservedPacket::Text(_)));
    }

    #[test]
    fn network_chunk_publisher_update_observation_reads_position_radius() {
        let mut payload = Vec::new();
        write_test_zigzag_i32(-116_096, &mut payload);
        write_test_zigzag_i32(79, &mut payload);
        write_test_zigzag_i32(-159_776, &mut payload);
        write_unsigned_varint_u32(64, &mut payload);
        payload.extend_from_slice(&0u32.to_le_bytes());

        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x79, &payload);
        let observed = observe_packet_ids(&stream).expect("observed packets");

        assert!(matches!(
            observed[0],
            ObservedPacket::NetworkChunkPublisherUpdate {
                x: -116_096,
                y: 79,
                z: -159_776,
                radius: 64
            }
        ));
    }

    #[test]
    fn level_chunk_observation_samples_runtime_paletted_subchunk_blocks() {
        let chunk_x = -7256;
        let chunk_z = -9987;
        let mut raw_payload = Vec::new();
        raw_payload.push(9); // subchunk version
        raw_payload.push(1); // layer count
        raw_payload.push(4); // subchunk y index, base y = 64
        raw_payload.push((1 << 1) | 1); // 1 bit per index, runtime palette
        let mut words = vec![0u32; 128];
        let block_index = 15 + 16 + 256; // local x=1,z=1,y=15 in YZX order
        words[block_index / 32] |= 1 << (block_index % 32);
        for word in words {
            raw_payload.extend_from_slice(&word.to_le_bytes());
        }
        write_unsigned_varint_u32(2, &mut raw_payload);
        write_unsigned_varint_u32(CHUNK_AIR_RUNTIME_ID, &mut raw_payload);
        write_unsigned_varint_u32(9852, &mut raw_payload);

        let mut payload = Vec::new();
        write_test_zigzag_i32(chunk_x, &mut payload);
        write_test_zigzag_i32(chunk_z, &mut payload);
        write_test_zigzag_i32(0, &mut payload);
        write_unsigned_varint_u32(1, &mut payload);
        payload.push(0); // cache disabled
        write_unsigned_varint_u32(raw_payload.len() as u32, &mut payload);
        payload.extend_from_slice(&raw_payload);

        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x3a, &payload);
        let observed = observe_packet_ids(&stream).expect("observed packets");

        let ObservedPacket::LevelChunk { samples, .. } = &observed[0] else {
            panic!("expected level chunk observation");
        };
        assert!(samples.iter().any(|sample| {
            sample.x == chunk_x * 16 + 1
                && sample.y == 79
                && sample.z == chunk_z * 16 + 1
                && sample.runtime_id == 9852
        }));
    }

    #[test]
    fn level_chunk_observation_keeps_samples_when_payload_reaches_trailing_storage() {
        let chunk_x = -7510;
        let chunk_z = -1501;
        let mut raw_payload = Vec::new();
        raw_payload.push(9); // subchunk version
        raw_payload.push(1); // layer count
        raw_payload.push(4); // subchunk y index, base y = 64
        raw_payload.push((1 << 1) | 1); // 1 bit per index, runtime palette
        let mut words = vec![0u32; 128];
        let block_index = 15 + 16 + 256; // local x=1,z=1,y=15 in YZX order
        words[block_index / 32] |= 1 << (block_index % 32);
        for word in words {
            raw_payload.extend_from_slice(&word.to_le_bytes());
        }
        write_unsigned_varint_u32(2, &mut raw_payload);
        write_unsigned_varint_u32(CHUNK_AIR_RUNTIME_ID, &mut raw_payload);
        write_unsigned_varint_u32(9852, &mut raw_payload);
        raw_payload.push(0x11); // trailing biome/storage data, not a terrain subchunk version.

        let mut payload = Vec::new();
        write_test_zigzag_i32(chunk_x, &mut payload);
        write_test_zigzag_i32(chunk_z, &mut payload);
        write_test_zigzag_i32(0, &mut payload);
        write_unsigned_varint_u32(2, &mut payload);
        payload.push(0); // cache disabled
        write_unsigned_varint_u32(raw_payload.len() as u32, &mut payload);
        payload.extend_from_slice(&raw_payload);

        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x3a, &payload);
        let observed = observe_packet_ids(&stream).expect("observed packets");

        let ObservedPacket::LevelChunk { samples, .. } = &observed[0] else {
            panic!("expected level chunk observation");
        };
        assert!(samples.iter().any(|sample| {
            sample.x == chunk_x * 16 + 1
                && sample.y == 79
                && sample.z == chunk_z * 16 + 1
                && sample.runtime_id == 9852
        }));
    }

    #[test]
    fn level_chunk_observation_decodes_singleton_storage_without_palette_length() {
        let chunk_x = -2268;
        let chunk_z = -8658;
        let mut raw_payload = Vec::new();
        raw_payload.push(9); // subchunk version
        raw_payload.push(2); // layer count
        raw_payload.push(4); // subchunk y index, base y = 64
        raw_payload.push((1 << 1) | 1); // 1 bit per index, runtime palette
        let mut words = vec![0u32; 128];
        let block_index = 15 + 16 + 256; // local x=1,z=1,y=15 in XZY order
        words[block_index / 32] |= 1 << (block_index % 32);
        for word in words {
            raw_payload.extend_from_slice(&word.to_le_bytes());
        }
        write_unsigned_varint_u32(2, &mut raw_payload);
        write_unsigned_varint_u32(CHUNK_AIR_RUNTIME_ID, &mut raw_payload);
        write_unsigned_varint_u32(9852, &mut raw_payload);
        raw_payload.push(1); // V0 singleton runtime palette; no palette length is encoded.
        write_unsigned_varint_u32(CHUNK_AIR_RUNTIME_ID, &mut raw_payload);

        raw_payload.push(9); // next subchunk must start exactly here.
        raw_payload.push(1);
        raw_payload.push(5); // base y = 80
        raw_payload.push(1); // V0 singleton runtime palette.
        write_unsigned_varint_u32(9877, &mut raw_payload);

        let mut payload = Vec::new();
        write_test_zigzag_i32(chunk_x, &mut payload);
        write_test_zigzag_i32(chunk_z, &mut payload);
        write_test_zigzag_i32(0, &mut payload);
        write_unsigned_varint_u32(2, &mut payload);
        payload.push(0); // cache disabled
        write_unsigned_varint_u32(raw_payload.len() as u32, &mut payload);
        payload.extend_from_slice(&raw_payload);

        let mut stream = Vec::new();
        append_test_packet(&mut stream, 0x3a, &payload);
        let observed = observe_packet_ids(&stream).expect("observed packets");

        let ObservedPacket::LevelChunk { samples, .. } = &observed[0] else {
            panic!("expected level chunk observation");
        };
        assert!(samples.iter().any(|sample| {
            sample.x == chunk_x * 16 + 1
                && sample.y == 79
                && sample.z == chunk_z * 16 + 1
                && sample.runtime_id == 9852
        }));
        assert!(samples.iter().any(|sample| {
            sample.x == chunk_x * 16 + 1
                && sample.y == 95
                && sample.z == chunk_z * 16 + 1
                && sample.runtime_id == 9877
        }));
    }

    #[test]
    fn level_chunk_subchunk_recovery_finds_expected_geyser_header() {
        let context = [
            0x86, 0xba, 0x01, 0xea, 0xd8, 0x01, 0x09, 0x01, 0xfd, 0x07, 0x92, 0x24, 0x49, 0x12,
            0x92, 0x24, 0x49, 0x12,
        ];
        let recovered = find_previous_subchunk_start(&context, 16, Some(-3));
        assert_eq!(recovered, Some(6));
        assert_eq!(find_previous_subchunk_start(&context, 16, Some(-2)), None);
    }

    fn append_test_packet(stream: &mut Vec<u8>, packet_id: u32, payload: &[u8]) {
        let mut packet = Vec::new();
        write_unsigned_varint_u32(packet_id, &mut packet);
        packet.extend_from_slice(payload);
        write_unsigned_varint_u32(packet.len() as u32, stream);
        stream.extend_from_slice(&packet);
    }

    fn write_test_zigzag_i32(value: i32, out: &mut Vec<u8>) {
        let encoded = ((value << 1) ^ (value >> 31)) as u32;
        write_unsigned_varint_u32(encoded, out);
    }

    fn write_test_zigzag_i64(value: i64, out: &mut Vec<u8>) {
        let encoded = ((value << 1) ^ (value >> 63)) as u64;
        write_test_var_u64(encoded, out);
    }

    fn write_test_var_u64(mut value: u64, out: &mut Vec<u8>) {
        loop {
            if (value & !0x7f) == 0 {
                out.push(value as u8);
                break;
            }
            out.push(((value & 0x7f) | 0x80) as u8);
            value >>= 7;
        }
    }

    #[test]
    fn play_status_string_representation() {
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(0),
            "LoginSuccess"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(1),
            "FailedClient (outdated client)"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(2),
            "FailedServer (outdated server)"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(3),
            "PlayerSpawn"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(4),
            "InvalidTenant"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(5),
            "EditionMismatchEduToVanilla"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(6),
            "EditionMismatchVanillaToEdu"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(7),
            "FailedMaxPlayers"
        );
        assert_eq!(
            BedrockProtocolAdapter::play_status_to_string(8),
            "FailedServerFull"
        );
        assert_eq!(BedrockProtocolAdapter::play_status_to_string(99), "Unknown");
    }

    #[test]
    fn login_handshake_play_status_failure_formatting() {
        let status = 1;
        let status_str = BedrockProtocolAdapter::play_status_to_string(status);
        let err_msg = format!(
            "Server returned PlayStatus failure: {} ({})",
            status, status_str
        );
        assert!(err_msg.contains("PlayStatus failure"));
        assert!(err_msg.contains("FailedClient (outdated client)"));
    }

    #[test]
    fn login_handshake_disconnect_offline_formatting() {
        let disc = torchflower_protocol::DisconnectPacket {
            reason: 0,
            hide_reason: false,
            message: Some("Invalid session".to_string()),
        };
        let err_msg = format!(
            "Server rejected offline/mock login (xuid=0). Real Xbox Live authentication is required. Disconnect reason={}, message={:?}",
            disc.reason,
            disc.message
        );
        assert!(err_msg.contains("rejected offline/mock login"));
        assert!(err_msg.contains("Real Xbox Live authentication is required"));
        assert!(err_msg.contains("Disconnect reason=0"));
        assert!(err_msg.contains("message=Some(\"Invalid session\")"));
    }

    #[test]
    fn login_handshake_disconnect_online_formatting() {
        let disc = torchflower_protocol::DisconnectPacket {
            reason: 3,
            hide_reason: true,
            message: Some("Spamming".to_string()),
        };
        let err_msg = format!(
            "Server disconnected during login handshake. Reason: {}, HideReason: {}, Message: {:?}",
            disc.reason, disc.hide_reason, disc.message
        );
        assert!(err_msg.contains("Server disconnected during login handshake"));
        assert!(err_msg.contains("Reason: 3"));
        assert!(err_msg.contains("HideReason: true"));
        assert!(err_msg.contains("Message: Some(\"Spamming\")"));
    }

    #[test]
    fn login_handshake_packet_log_classification() {
        use torchflower_protocol::{
            DisconnectPacket, Packet, PlayStatusPacket, ServerToClientHandshakePacket,
        };

        let p1 = Packet::PlayStatus(PlayStatusPacket { status: 0 });
        BedrockProtocolAdapter::log_handshake_packet(&p1);

        let p2 = Packet::Disconnect(DisconnectPacket {
            reason: 1,
            hide_reason: false,
            message: Some("Testing".to_string()),
        });
        BedrockProtocolAdapter::log_handshake_packet(&p2);

        let p3 = Packet::ServerToClientHandshake(ServerToClientHandshakePacket {
            handshake_web_token: "jwt_token".to_string(),
        });
        BedrockProtocolAdapter::log_handshake_packet(&p3);
    }
}
