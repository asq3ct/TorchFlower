# torchflower-world

A small Bedrock world model for TorchFlower bots. It keeps memory use per bot bounded.

- `SparseWorld` is a toroidal window of `(2r+1)²` chunk columns around the bot. The default `r` is 2.
  - Sub-chunks within `hot_radius` of the bot's Y keep their full paletted data.
  - Sub-chunks further away are reduced to 512-byte collision masks.
  - Columns that leave the window are dropped straight away.
- `LevelChunk` payloads are decoded in both inline and sub-chunk-request modes. `SubChunk` payloads are decoded too, using the protocol < 2168 layout. The crate also encodes `SubChunkRequest`, in both the pre-1001 and the 1001 layouts.
- Storage is re-packed to the smallest valid bit width when it is decoded.
- `BlockRegistry` resolves runtime ids in either sequential mode (sorted by FNV-1 64) or hashed mode (FNV-1a 32).
  - The vanilla palettes for protocols 766–1001 (Bedrock 1.21.60–1.26.30) are embedded, so `BlockRegistry::vanilla(protocol, mode)` works without any external files. They are generated from [pmmp/BedrockData](https://github.com/pmmp/BedrockData) (CC0) by `tools/gen_palettes.py` in a compact form of about 14 KB per version.
  - For servers with a different palette, load their `canonical_block_states.nbt` with `BlockRegistry::from_canonical_nbt`.
  - `BlockRegistry::fallback` recognises only air, water and lava (hashed mode) and is used only if no palette can be decoded.
- Block properties (hardness, friction, light) come from a table generated from BedrockData's `block_properties_table.json` (CC0). Regenerate it with `tools/gen_block_table.py`.
- Collision shapes and preferred tools are guessed from block names and states.
