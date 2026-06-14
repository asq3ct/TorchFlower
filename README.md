# TorchFlower

[![Crates.io](https://img.shields.io/badge/crates.io-unpublished-lightgrey)](https://crates.io/crates/torchflower)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)
[![CI](https://github.com/Osamu-GWAD/TorchFlower/actions/workflows/ci.yml/badge.svg)](.github/workflows/)
[![Discord](https://img.shields.io/badge/Discord-Join-5865F2?logo=discord&logoColor=white)](/discord.gg/u4Kfe6MQjb)

> **Status:** Early alpha — `v0.1.0`. Expect breaking changes between minor versions.
> See [CHANGELOG.md](./CHANGELOG.md) for what has changed.

TorchFlower is a Rust-first Minecraft Bedrock bot engine for authenticated sessions, scalable bot orchestration, real-server validation, protocol work, and guarded automation. Unlike a standalone protocol client or protocol crate, TorchFlower packages authentication, RakNet transport, session behavior, REST control, persistence, world utilities, addon helpers, metrics, and safety defaults into one modular workspace while preserving the validated Bedrock login and gameplay pipeline.

## Quickstart

Get a bot authenticated and running in five steps.(Is being worked on as the auth breaks sometimes.)

**Step 1 — Configure the engine**

```powershell
Copy-Item .env.example .env
# Open .env and fill in at minimum:
#   TORCHFLOWER_API_KEY=<any random string>
#   TOKEN_ENCRYPTION_KEY_B64=<base64-encoded 32 random bytes>
```

Generate a suitable encryption key with:

```powershell
[Convert]::ToBase64String((1..32 | ForEach-Object { Get-Random -Maximum 256 }))
```

**Step 2 — Start the engine** *(terminal 1)*

```powershell
cargo run -p torchflower-engine
```

**Step 3 — Log in with your Microsoft account** *(terminal 2)*

```powershell
.\scripts\login-and-save.ps1 -Email "your@email.com"
```

**Step 4 — Complete the Microsoft login in your browser**

The script prints a URL and a short code. Open the URL, enter the code, and
sign in with the Microsoft account that owns Minecraft Bedrock. The script
polls automatically and saves the session file when done.

**Step 5 — Run the lite-bot**

```powershell
torchflower-lite-bot run --config bots.toml
```

> **Tip:** To re-authenticate (e.g. after token expiry) re-run Step 3.
> The script overwrites the session file in place — no database changes needed.

---

## Quick Start (library API)

```rust
use torchflower::{AuthConfig, BotBuilder, Event, ProtocolVersion};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let bot = BotBuilder::new()
    .address("play.example.com", 19132)
    .protocol_version(ProtocolVersion::V1_21_100)
    .auth(AuthConfig::device_code())
    .build()
    .await?;

bot.run(|ctx, event| {
    Box::pin(async move {
        if let Event::Spawned = event {
            ctx.send_chat("Hello from TorchFlower!").await?;
        }
        Ok(())
    })
}).await?;
# Ok(())
# }
```

Run the workspace quickstart example:

```powershell
$env:MINECRAFT_HOST="play.example.com"
$env:MINECRAFT_PORT="19132"
cargo run --example quickstart
```

## Crates

| Crate | Purpose | crates.io |
|---|---|---|
| `torchflower` | Facade re-export crate | unpublished |
| `torchflower-auth` | Storage-agnostic Microsoft, Xbox, PlayFab, entitlement, and Bedrock auth types | unpublished |
| `torchflower-proto` | Bedrock packet models, form data, protocol versions, and codecs | unpublished |
| `torchflower-net` | RakNet transport, ACK/NACK, fragmentation, and reassembly | unpublished |
| `torchflower-engine` | Bot session, persistence, diagnostics, validation, and gameplay actions | unpublished |
| `torchflower-api` | Authenticated REST API wrapper around the engine | unpublished |
| `torchflower-level` | Bedrock world-folder and LevelDB key utilities | unpublished |
| `torchflower-addon` | Addon manifest and `.mcpack`/`.mcaddon` helpers | unpublished |

## Feature Flags

| Feature | Crate | Description |
|---|---|---|
| `offline-mode` | `torchflower`, `torchflower-auth` | Enables local/offline auth config for local servers that do not require online-mode authentication. |
| `level` | `torchflower` | Re-exports `torchflower-level`. |
| `addon` | `torchflower` | Re-exports `torchflower-addon`. |
| `console` | `torchflower`, `torchflower-engine` | Enables tokio-console runtime instrumentation. |

## Authentication

The engine implements the complete Bedrock account path:

1. Microsoft device-code OAuth using the Live flow by default
2. Xbox Live authentication
3. Standard XSTS authentication
4. PlayFab XSTS authentication with `RP=http://playfab.xboxlive.com/`
5. PlayFab `LoginWithXbox` against `https://20ca2.playfabapi.com/Client/LoginWithXbox` with `CreateAccount=true`
6. Minecraft entitlement session initialization against `https://authorization.franchise.minecraft-services.net/api/v1.0/session/start`
7. Legacy Bedrock authentication against `https://multiplayer.minecraft.net/authentication`
8. Bedrock JWT chain generation

Tokens and provisioning state are persisted by `torchflower-engine`; `torchflower-auth` keeps public auth data structures storage-agnostic.

## Capabilities

- RakNet handshake, ACK/NACK handling, fragmentation, and reassembly
- RequestNetworkSettings and ZLib negotiation
- Login packet generation, compression, encryption handshake, and encrypted batch decoding
- Resource pack acknowledgement and client cache status
- StartGame, player spawn, keepalive, chat, inventory observation, movement, and disconnect handling
- NetworkStackLatency response encoding
- Server-confirmed block breaking evidence through UpdateBlock observation
- Guarded block placing after a normal placeable item is confirmed in inventory
- Authenticated REST API with exact-origin CORS and safe diagnostics defaults

Server menu, teleport, region-selector, and UI-tool items are rejected as placeable inventory even when their raw item id resembles a block.

## Configuration

Copy `.env.example` to `.env` and set values as needed:

```powershell
MICROSOFT_AUTH_FLOW=live
MICROSOFT_CLIENT_ID=
TOKEN_ENCRYPTION_KEY_B64=replace-with-base64-encoded-32-random-bytes
DATABASE_URL=sqlite://database/torchflower.sqlite
RUST_ENGINE_BIND=127.0.0.1:9080
LOG_LEVEL=info
TORCHFLOWER_API_KEY=replace-with-random-api-key
TORCHFLOWER_CORS_ALLOWED_ORIGINS=http://localhost:3000,http://127.0.0.1:3000
TORCHFLOWER_ALLOWED_SERVER_HOSTS=example.org
BEDROCK_VALIDATE_ACCOUNT_ID=<account-id>
BEDROCK_VALIDATE_SERVER_HOST=<server-host>
BEDROCK_VALIDATE_SERVER_PORT=19132
BEDROCK_VALIDATE_DURATION_SECONDS=300
```

`MICROSOFT_CLIENT_ID` is optional in Live mode. Keep `TOKEN_ENCRYPTION_KEY_B64` stable for the database; changing it makes stored token ciphertext undecryptable.

## API Reference

Full API documentation will be published at `https://docs.rs/torchflower` after the first crates.io release.

## Relationship to bedrock-rs / rak-rs

TorchFlower builds on top of [`rak-rs`](https://github.com/bedrock-crustaceans/rak-rs)
for RakNet transport. We actively contribute fixes and improvements back
upstream rather than maintaining a private fork.

Pending or merged upstream contributions from TorchFlower are tracked in
[docs/upstreaming-bedrock-rs.md](./docs/upstreaming-bedrock-rs.md).

## Performance And Scale

TorchFlower is tuned for maximum concurrent bots with bounded task creation, command-channel back-pressure, configurable auth concurrency, spawn pacing, and reusable packet buffers. Runtime tuning is available through:

```bash
TORCHFLOWER_WORKERS=4
TORCHFLOWER_THREAD_STACK_BYTES=2097152
TORCHFLOWER_MAX_BOTS=100
TORCHFLOWER_MAX_AUTH_CONCURRENT=3
TORCHFLOWER_SPAWN_INTERVAL_MS=500
```

Tokio console can be enabled for runtime analysis:

```bash
TORCHFLOWER_WORKERS=4 cargo run -p torchflower-engine --features console
tokio-console
```

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). Join the placeholder Discord at [discord.gg/placeholder](https://discord.gg/placeholder); replace this link with the project server before public release.

## License

Licensed under the [Apache License, Version 2.0](./LICENSE).
