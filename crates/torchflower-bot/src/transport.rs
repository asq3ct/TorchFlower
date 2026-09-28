//! Connection abstraction.
//!
//! [`Transport`] moves *decompressed, decrypted* packet batches. The engine
//! implementation ([`EngineTransport`]) reuses TorchFlower's validated RakNet,
//! login, encryption and compression pipeline; tests use an in-memory
//! implementation.

use std::future::Future;

use torchflower_engine::auth::ProvisionedBedrockSession;
use torchflower_engine::bedrock::local_network::codec;
use torchflower_engine::bedrock::protocol_adapter::{
    BedrockProtocolAdapter, BedrockProtocolOptions,
};
use torchflower_protocol::{Packet, ProtocolVersion};

use crate::error::{BotError, BotResult};

/// Packet batch transport.
pub trait Transport: Send + 'static {
    /// Receives the next batch of length-prefixed packets.
    fn recv(&mut self) -> impl Future<Output = BotResult<Vec<u8>>> + Send;
    /// Sends a batch of length-prefixed packets.
    fn send(&mut self, batch: Vec<u8>) -> impl Future<Output = BotResult<()>> + Send;
    /// Sends packets that are encoded by `torchflower-protocol`.
    fn send_typed(&mut self, packets: Vec<Packet>) -> impl Future<Output = BotResult<()>> + Send;
    /// Answers a `NetworkStackLatency` request.
    fn respond_latency(&mut self, timestamp: i64) -> impl Future<Output = BotResult<()>> + Send;
    /// Negotiated protocol version.
    fn protocol(&self) -> i32;
    /// Closes the connection.
    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

fn engine_err(e: impl std::fmt::Display) -> BotError {
    BotError::Transport(e.to_string())
}

/// Transport backed by the engine's [`BedrockProtocolAdapter`].
pub struct EngineTransport {
    conn: BedrockProtocolAdapter,
    protocol: i32,
    pending: Option<Vec<u8>>,
}

impl EngineTransport {
    /// Connects, negotiates network settings and completes the login
    /// handshake (including encryption when the server requests it).
    pub async fn connect(
        host: &str,
        port: u16,
        session: &ProvisionedBedrockSession,
        options: BedrockProtocolOptions,
    ) -> BotResult<Self> {
        let protocol = options.requested_protocol_version;
        let mut conn = BedrockProtocolAdapter::connect_with_options(host, port, options)
            .await
            .map_err(engine_err)?;
        conn.request_network_settings().await.map_err(engine_err)?;
        conn.send_login(session).await.map_err(engine_err)?;
        let (pending, _encrypted) = conn
            .complete_login_handshake(&session.chain)
            .await
            .map_err(engine_err)?;
        // Packets decoded during the handshake are re-encoded so the bot sees
        // a uniform raw stream.
        let pending = if pending.is_empty() {
            None
        } else {
            Some(
                codec::encode_packets(&pending, None, None, ProtocolVersion::V898)
                    .map_err(engine_err)?,
            )
        };
        Ok(Self {
            conn,
            protocol,
            pending,
        })
    }
}

impl Transport for EngineTransport {
    async fn recv(&mut self) -> BotResult<Vec<u8>> {
        if let Some(p) = self.pending.take() {
            return Ok(p);
        }
        self.conn.recv_raw().await.map_err(engine_err)
    }

    async fn send(&mut self, batch: Vec<u8>) -> BotResult<()> {
        if batch.is_empty() {
            return Ok(());
        }
        self.conn
            .send_preencoded_packet_stream("torchflower_bot", batch)
            .await
            .map_err(engine_err)
    }

    async fn send_typed(&mut self, packets: Vec<Packet>) -> BotResult<()> {
        self.conn.send(&packets).await.map_err(engine_err)
    }

    async fn respond_latency(&mut self, timestamp: i64) -> BotResult<()> {
        self.conn
            .send_network_stack_latency_response(timestamp as u64)
            .await
            .map_err(engine_err)
    }

    fn protocol(&self) -> i32 {
        self.protocol
    }

    async fn close(&mut self) {
        self.conn.close().await;
    }
}

/// In-memory transport for tests and simulations.
pub mod memory {
    use super::*;
    use tokio::sync::mpsc;

    /// Everything the bot sent.
    #[derive(Debug)]
    pub enum Sent {
        Raw(Vec<u8>),
        Typed(Vec<Packet>),
        Latency(i64),
    }

    /// Bot side of an in-memory connection.
    pub struct MemoryTransport {
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
        tx: mpsc::UnboundedSender<Sent>,
        protocol: i32,
    }

    /// Server side of an in-memory connection.
    pub struct MemoryServer {
        pub to_bot: mpsc::UnboundedSender<Vec<u8>>,
        pub from_bot: mpsc::UnboundedReceiver<Sent>,
    }

    /// Creates a connected pair.
    pub fn pair(protocol: i32) -> (MemoryTransport, MemoryServer) {
        let (to_bot, rx) = mpsc::unbounded_channel();
        let (tx, from_bot) = mpsc::unbounded_channel();
        (
            MemoryTransport { rx, tx, protocol },
            MemoryServer { to_bot, from_bot },
        )
    }

    impl Transport for MemoryTransport {
        async fn recv(&mut self) -> BotResult<Vec<u8>> {
            self.rx.recv().await.ok_or(BotError::Disconnected)
        }
        async fn send(&mut self, batch: Vec<u8>) -> BotResult<()> {
            if batch.is_empty() {
                return Ok(());
            }
            self.tx
                .send(Sent::Raw(batch))
                .map_err(|_| BotError::Disconnected)
        }
        async fn send_typed(&mut self, packets: Vec<Packet>) -> BotResult<()> {
            self.tx
                .send(Sent::Typed(packets))
                .map_err(|_| BotError::Disconnected)
        }
        async fn respond_latency(&mut self, timestamp: i64) -> BotResult<()> {
            self.tx
                .send(Sent::Latency(timestamp))
                .map_err(|_| BotError::Disconnected)
        }
        fn protocol(&self) -> i32 {
            self.protocol
        }
        async fn close(&mut self) {
            self.rx.close();
        }
    }
}
