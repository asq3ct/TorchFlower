# torchflower-world

A small Bedrock world model for TorchFlower bots. It keeps memory use per bot bounded.

- `SparseWorld` is a toroidal window of `(2r+1)²` chunk columns around the bot. The default `r` is 2.
  - Sub-chunks within `hot_radius` of the bot's Y keep their full paletted data.
  - Sub-chunks further away are reduced to 512-byte collision masks.
  - Columns that leave the window are dropped straight away.
- `LevelChunk` payloads are decoded in both inline and sub-chunk-request modes. `SubChunk` payloads are decoded too, using the protocol < 2168 layout. The crate also encodes `SubChunkRequest`.
- Storage is re-packed to the smallest valid bit width when it is decoded.
- `BlockRegistry` resolves runtime ids in either sequential mode (sorted by FNV-1 64) or hashed mode (FNV-1a 32).
  - Load `canonical_block_states.nbt` from [pmmp/BedrockData](https://github.com/pmmp/BedrockData) for your server's version with `BlockRegistry::from_canonical_nbt`.
  - Without that file, `BlockRegistry::fallback` recognises only air, water and lava, and only in hashed mode. Every other block is treated as an unknown solid cube.
- Block properties (hardness, friction, light) come from a table generated from BedrockData's `block_properties_table.json` (CC0). Regenerate it with `tools/gen_block_table.py`.
- Collision shapes and preferred tools are guessed from block names and states.
