//! Block semantics: collision shapes, material flags, tools and dig timing.
//!
//! Numeric properties (hardness, friction, light) come from the bundled,
//! generated `block_table` module (pmmp/BedrockData, CC0). Collision shapes,
//! liquid/climbable flags and preferred tools are derived from block names and
//! block states using conservative heuristics; unknown blocks are treated as
//! solid full cubes so that physics and pathfinding stay safe.

use crate::block_table::BLOCK_TABLE;
use torchflower_protocol_core::wire::NbtValue;

/// Bit flags describing block material behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BlockFlags(pub u16);

impl BlockFlags {
    /// No flags.
    pub const NONE: Self = Self(0);
    /// Has a collision box.
    pub const SOLID: Self = Self(1 << 0);
    /// Water-like or lava-like liquid.
    pub const LIQUID: Self = Self(1 << 1);
    /// Water or bubble column.
    pub const WATER: Self = Self(1 << 2);
    /// Lava.
    pub const LAVA: Self = Self(1 << 3);
    /// Ladders, vines, scaffolding.
    pub const CLIMBABLE: Self = Self(1 << 4);
    /// A block can be placed into this position (air, grass, liquids, ...).
    pub const REPLACEABLE: Self = Self(1 << 5);
    /// Harmful to stand in or on (lava, fire, cactus, magma, ...).
    pub const DANGEROUS: Self = Self(1 << 6);
    /// Slows movement (cobweb, sweet berries, powder snow, honey, soul sand).
    pub const SLOW: Self = Self(1 << 7);
    /// Falls when unsupported (sand, gravel, concrete powder).
    pub const GRAVITY: Self = Self(1 << 8);
    /// Opens a UI or toggles when interacted with.
    pub const INTERACTABLE: Self = Self(1 << 9);
    /// Cannot be broken in survival (bedrock, barrier, portals, ...).
    pub const UNBREAKABLE: Self = Self(1 << 10);
    /// Runtime id unknown to the registry.
    pub const UNKNOWN: Self = Self(1 << 11);
    /// Air (including cave/void air).
    pub const AIR: Self = Self(1 << 12);

    /// Returns true if all bits of `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Bitwise union.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Removes `other` bits.
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl std::ops::BitOr for BlockFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// Collision shape of a block state in 1/16th block units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shape {
    /// No collision.
    Empty,
    /// Full-width box from `lo/16` to `hi/16` blocks high (`hi` may exceed 16, e.g. fences are 24).
    Box {
        /// Bottom of the box in 1/16 blocks.
        lo: u8,
        /// Top of the box in 1/16 blocks.
        hi: u8,
    },
    /// Stairs: bottom (or top when `upside_down`) half plus a quarter step.
    Stairs {
        /// Bedrock `weirdo_direction`: 0 east, 1 west, 2 south, 3 north.
        dir: u8,
        /// True if the slab half is at the top.
        upside_down: bool,
    },
}

impl Shape {
    /// Full cube.
    pub const FULL: Shape = Shape::Box { lo: 0, hi: 16 };

    /// True if the shape has no collision.
    pub fn is_empty(self) -> bool {
        matches!(self, Shape::Empty)
    }

    /// True if the shape is exactly a full cube.
    pub fn is_full(self) -> bool {
        self == Shape::FULL
    }

    /// Height of the top collision surface in blocks (0 when empty).
    pub fn top(self) -> f32 {
        match self {
            Shape::Empty => 0.0,
            Shape::Box { hi, .. } => hi as f32 / 16.0,
            Shape::Stairs { .. } => 1.0,
        }
    }

    /// Collision boxes as `[min_x, min_y, min_z, max_x, max_y, max_z]` in
    /// block-local coordinates. Returns the boxes and how many are valid.
    pub fn boxes(self) -> ([[f32; 6]; 2], usize) {
        let mut out = [[0.0; 6]; 2];
        match self {
            Shape::Empty => (out, 0),
            Shape::Box { lo, hi } => {
                out[0] = [0.0, lo as f32 / 16.0, 0.0, 1.0, hi as f32 / 16.0, 1.0];
                (out, 1)
            }
            Shape::Stairs { dir, upside_down } => {
                let (slab_lo, slab_hi, step_lo, step_hi) = if upside_down {
                    (0.5, 1.0, 0.0, 0.5)
                } else {
                    (0.0, 0.5, 0.5, 1.0)
                };
                out[0] = [0.0, slab_lo, 0.0, 1.0, slab_hi, 1.0];
                let (x0, x1, z0, z1) = match dir {
                    0 => (0.5, 1.0, 0.0, 1.0),
                    1 => (0.0, 0.5, 0.0, 1.0),
                    2 => (0.0, 1.0, 0.5, 1.0),
                    _ => (0.0, 1.0, 0.0, 0.5),
                };
                out[1] = [x0, step_lo, z0, x1, step_hi, z1];
                (out, 2)
            }
        }
    }
}

/// Tool category that mines a block efficiently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKind {
    /// No tool is preferred.
    None,
    /// Pickaxe.
    Pickaxe,
    /// Axe.
    Axe,
    /// Shovel.
    Shovel,
    /// Hoe.
    Hoe,
    /// Shears.
    Shears,
    /// Sword.
    Sword,
}

/// Tool material tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolTier {
    /// Wooden tools.
    Wood,
    /// Golden tools.
    Gold,
    /// Stone tools.
    Stone,
    /// Copper tools.
    Copper,
    /// Iron tools.
    Iron,
    /// Diamond tools.
    Diamond,
    /// Netherite tools.
    Netherite,
}

impl ToolTier {
    /// Base mining speed multiplier.
    pub fn speed(self) -> f32 {
        match self {
            ToolTier::Wood => 2.0,
            ToolTier::Stone => 4.0,
            ToolTier::Copper => 5.0,
            ToolTier::Iron => 6.0,
            ToolTier::Diamond => 8.0,
            ToolTier::Netherite => 9.0,
            ToolTier::Gold => 12.0,
        }
    }

    /// Harvest level: 0 wood/gold, 1 stone/copper, 2 iron, 3 diamond, 4 netherite.
    pub fn level(self) -> u8 {
        match self {
            ToolTier::Wood | ToolTier::Gold => 0,
            ToolTier::Stone | ToolTier::Copper => 1,
            ToolTier::Iron => 2,
            ToolTier::Diamond => 3,
            ToolTier::Netherite => 4,
        }
    }
}

/// A concrete tool held by the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tool {
    /// Tool category.
    pub kind: ToolKind,
    /// Tool material tier.
    pub tier: ToolTier,
    /// Efficiency enchantment level.
    pub efficiency: u8,
}

impl Tool {
    /// Parses a Bedrock item identifier such as `minecraft:iron_pickaxe`.
    pub fn from_item_name(name: &str) -> Option<Tool> {
        let name = name.strip_prefix("minecraft:").unwrap_or(name);
        if name == "shears" {
            return Some(Tool {
                kind: ToolKind::Shears,
                tier: ToolTier::Iron,
                efficiency: 0,
            });
        }
        let (tier, kind) = name.split_once('_')?;
        let tier = match tier {
            "wooden" => ToolTier::Wood,
            "stone" => ToolTier::Stone,
            "copper" => ToolTier::Copper,
            "iron" => ToolTier::Iron,
            "golden" => ToolTier::Gold,
            "diamond" => ToolTier::Diamond,
            "netherite" => ToolTier::Netherite,
            _ => return None,
        };
        let kind = match kind {
            "pickaxe" => ToolKind::Pickaxe,
            "axe" => ToolKind::Axe,
            "shovel" => ToolKind::Shovel,
            "hoe" => ToolKind::Hoe,
            "sword" => ToolKind::Sword,
            _ => return None,
        };
        Some(Tool {
            kind,
            tier,
            efficiency: 0,
        })
    }
}

/// Per-name static block information.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockInfo {
    /// Name without the `minecraft:` prefix.
    pub name: &'static str,
    /// Hardness; negative means unbreakable.
    pub hardness: f32,
    /// Surface friction (slipperiness): 0.6 default, 0.98 ice, 0.8 slime.
    pub friction: f32,
    /// Light emission 0..=15.
    pub light: u8,
}

/// Looks up bundled properties for a block name (with or without namespace).
pub fn block_info(name: &str) -> Option<&'static BlockInfo> {
    static INFOS: std::sync::OnceLock<Vec<BlockInfo>> = std::sync::OnceLock::new();
    let infos = INFOS.get_or_init(|| {
        BLOCK_TABLE
            .iter()
            .map(|&(name, hardness, friction, light)| BlockInfo {
                name,
                hardness,
                friction,
                light,
            })
            .collect()
    });
    let short = name.strip_prefix("minecraft:").unwrap_or(name);
    infos
        .binary_search_by(|info| info.name.cmp(short))
        .ok()
        .map(|i| &infos[i])
}

/// Derived static characteristics of a block name.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Material {
    /// Material flags.
    pub flags: BlockFlags,
    /// Default collision shape.
    pub shape: Shape,
    /// Tool that mines the block fastest.
    pub tool: ToolKind,
    /// Minimum [`ToolTier::level`] (plus one) required for drops; 0 = hand ok.
    pub harvest: u8,
}

fn any_suffix(name: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|s| name.ends_with(s))
}

fn any_contains(name: &str, parts: &[&str]) -> bool {
    parts.iter().any(|s| name.contains(s))
}

const WOOD_NAMES: &[&str] = &[
    "oak", "spruce", "birch", "jungle", "acacia", "dark_oak", "mangrove", "cherry", "bamboo",
    "crimson", "warped", "pale_oak", "wooden",
];

/// Classifies a block by name (namespace optional).
pub fn classify(name: &str) -> Material {
    let n = name.strip_prefix("minecraft:").unwrap_or(name);
    let mut flags = BlockFlags::SOLID;
    let mut shape = Shape::FULL;

    let no_collision = matches!(
        n,
        "air"
            | "cave_air"
            | "void_air"
            | "structure_void"
            | "light_block"
            | "fire"
            | "soul_fire"
            | "short_grass"
            | "tall_grass"
            | "grass"
            | "fern"
            | "large_fern"
            | "deadbush"
            | "dead_bush"
            | "seagrass"
            | "kelp"
            | "vine"
            | "redstone_wire"
            | "tripwire"
            | "lever"
            | "reeds"
            | "sugar_cane"
            | "wheat"
            | "carrots"
            | "potatoes"
            | "beetroot"
            | "melon_stem"
            | "pumpkin_stem"
            | "nether_wart"
            | "sweet_berry_bush"
            | "cobweb"
            | "web"
            | "glow_lichen"
            | "sculk_vein"
            | "hanging_roots"
            | "spore_blossom"
            | "cave_vines"
            | "cave_vines_body_with_berries"
            | "cave_vines_head_with_berries"
            | "weeping_vines"
            | "twisting_vines"
            | "small_dripleaf_block"
            | "pink_petals"
            | "nether_sprouts"
            | "crimson_roots"
            | "warped_roots"
            | "brown_mushroom"
            | "red_mushroom"
            | "crimson_fungus"
            | "warped_fungus"
            | "dandelion"
            | "poppy"
            | "blue_orchid"
            | "allium"
            | "azure_bluet"
            | "oxeye_daisy"
            | "cornflower"
            | "lily_of_the_valley"
            | "wither_rose"
            | "torchflower"
            | "torchflower_crop"
            | "pitcher_plant"
            | "pitcher_crop"
            | "sunflower"
            | "lilac"
            | "rose_bush"
            | "peony"
            | "red_flower"
            | "yellow_flower"
            | "double_plant"
            | "tallgrass"
            | "powder_snow"
            | "end_portal"
            | "portal"
            | "end_gateway"
            | "bubble_column"
            | "frog_spawn"
            | "leaf_litter"
            | "wildflowers"
            | "bush"
            | "firefly_bush"
            | "short_dry_grass"
            | "tall_dry_grass"
            | "cactus_flower"
            | "resin_clump"
            | "closed_eyeblossom"
            | "open_eyeblossom"
    ) || any_suffix(
        n,
        &[
            "_sapling",
            "_tulip",
            "torch",
            "_button",
            "_pressure_plate",
            "_sign",
            "_banner",
            "rail",
            "_coral",
            "_coral_fan",
            "_coral_wall_fan",
            "_propagule",
        ],
    ) && !n.ends_with("_coral_block");
    if no_collision {
        flags = flags.without(BlockFlags::SOLID);
        shape = Shape::Empty;
    }
    if matches!(
        n,
        "air" | "cave_air" | "void_air" | "structure_void" | "light_block"
    ) {
        flags = flags | BlockFlags::AIR | BlockFlags::REPLACEABLE;
    }
    if matches!(
        n,
        "short_grass"
            | "tall_grass"
            | "grass"
            | "tallgrass"
            | "fern"
            | "large_fern"
            | "deadbush"
            | "dead_bush"
            | "seagrass"
            | "vine"
            | "fire"
            | "soul_fire"
            | "glow_lichen"
            | "double_plant"
            | "snow_layer"
            | "leaf_litter"
            | "short_dry_grass"
            | "tall_dry_grass"
            | "bush"
    ) {
        flags = flags | BlockFlags::REPLACEABLE;
    }

    // Liquids.
    if matches!(n, "water" | "flowing_water" | "bubble_column") {
        flags = BlockFlags::LIQUID | BlockFlags::WATER | BlockFlags::REPLACEABLE;
        shape = Shape::Empty;
    }
    if matches!(n, "lava" | "flowing_lava") {
        flags =
            BlockFlags::LIQUID | BlockFlags::LAVA | BlockFlags::REPLACEABLE | BlockFlags::DANGEROUS;
        shape = Shape::Empty;
    }

    // Climbables.
    if matches!(
        n,
        "ladder"
            | "vine"
            | "scaffolding"
            | "weeping_vines"
            | "twisting_vines"
            | "cave_vines"
            | "cave_vines_body_with_berries"
            | "cave_vines_head_with_berries"
    ) {
        flags = flags.without(BlockFlags::SOLID) | BlockFlags::CLIMBABLE;
        shape = Shape::Empty;
    }

    // Partial heights (sixteenths).
    let partial = match n {
        _ if n.ends_with("carpet") => Some(1),
        "snow_layer" => Some(2),
        "lily_pad" | "waterlily" => Some(1),
        "farmland" | "grass_path" | "dirt_path" | "honey_block" | "mud" => Some(15),
        "soul_sand" => Some(14),
        "chest" | "trapped_chest" | "ender_chest" => Some(14),
        "enchanting_table" | "stonecutter_block" => Some(12),
        "end_portal_frame" => Some(13),
        "cake" => Some(8),
        "daylight_detector" | "daylight_detector_inverted" => Some(6),
        "bed" => Some(9),
        "cactus" => Some(15),
        "sculk_sensor" | "calibrated_sculk_sensor" | "sculk_shrieker" => Some(8),
        "flower_pot" | "candle" => Some(6),
        "lantern" | "soul_lantern" => Some(9),
        "campfire" | "soul_campfire" => Some(7),
        "repeater"
        | "unpowered_repeater"
        | "powered_repeater"
        | "comparator"
        | "unpowered_comparator"
        | "powered_comparator" => Some(2),
        _ if n.ends_with("_slab") && !n.contains("double") => Some(8),
        _ if n.ends_with("_candle") => Some(6),
        _ if n.ends_with("_bed") => Some(9),
        _ => None,
    };
    if let Some(hi) = partial {
        shape = Shape::Box { lo: 0, hi };
    }
    if any_suffix(n, &["fence", "_wall", "fence_gate"]) || n == "border_block" {
        shape = Shape::Box { lo: 0, hi: 24 };
    }
    if n.ends_with("_stairs") {
        shape = Shape::Stairs {
            dir: 0,
            upside_down: false,
        };
    }

    // Hazards / slowness.
    if matches!(
        n,
        "fire"
            | "soul_fire"
            | "magma"
            | "cactus"
            | "sweet_berry_bush"
            | "wither_rose"
            | "campfire"
            | "soul_campfire"
            | "pointed_dripstone"
    ) {
        flags = flags | BlockFlags::DANGEROUS;
    }
    if matches!(
        n,
        "cobweb" | "web" | "sweet_berry_bush" | "powder_snow" | "honey_block" | "soul_sand"
    ) {
        flags = flags | BlockFlags::SLOW;
    }
    if matches!(
        n,
        "sand"
            | "red_sand"
            | "gravel"
            | "suspicious_sand"
            | "suspicious_gravel"
            | "anvil"
            | "dragon_egg"
            | "scaffolding"
    ) || n.ends_with("concrete_powder")
    {
        flags = flags | BlockFlags::GRAVITY;
    }
    if any_suffix(
        n,
        &[
            "chest",
            "barrel",
            "furnace",
            "smoker",
            "crafting_table",
            "_door",
            "_trapdoor",
            "fence_gate",
            "shulker_box",
            "hopper",
            "dispenser",
            "dropper",
            "anvil",
            "lever",
            "_button",
            "_bed",
            "enchanting_table",
            "brewing_stand",
            "loom",
            "stonecutter_block",
            "grindstone",
            "smithing_table",
            "cartography_table",
            "beacon",
            "crafter",
        ],
    ) || n == "bed"
        || n == "trapdoor"
        || n == "noteblock"
    {
        flags = flags | BlockFlags::INTERACTABLE;
    }
    if matches!(
        n,
        "bedrock"
            | "barrier"
            | "end_portal"
            | "end_portal_frame"
            | "portal"
            | "end_gateway"
            | "command_block"
            | "chain_command_block"
            | "repeating_command_block"
            | "structure_block"
            | "jigsaw"
            | "invisible_bedrock"
            | "border_block"
            | "allow"
            | "deny"
            | "light_block"
    ) || n.starts_with("light_block_")
    {
        flags = flags | BlockFlags::UNBREAKABLE;
    }

    let (tool, harvest) = tool_for(n);
    Material {
        flags,
        shape,
        tool,
        harvest,
    }
}

fn tool_for(n: &str) -> (ToolKind, u8) {
    // Harvest tiers encoded as required level + 1 (0 = any).
    const DIAMOND: u8 = 4;
    const IRON: u8 = 3;
    const STONE: u8 = 2;
    const WOOD: u8 = 1;
    match n {
        "obsidian" | "crying_obsidian" | "ancient_debris" | "netherite_block"
        | "respawn_anchor" => return (ToolKind::Pickaxe, DIAMOND),
        "gold_ore"
        | "deepslate_gold_ore"
        | "diamond_ore"
        | "deepslate_diamond_ore"
        | "emerald_ore"
        | "deepslate_emerald_ore"
        | "redstone_ore"
        | "lit_redstone_ore"
        | "deepslate_redstone_ore"
        | "lit_deepslate_redstone_ore"
        | "gold_block"
        | "diamond_block"
        | "emerald_block" => return (ToolKind::Pickaxe, IRON),
        "iron_ore"
        | "deepslate_iron_ore"
        | "lapis_ore"
        | "deepslate_lapis_ore"
        | "copper_ore"
        | "deepslate_copper_ore"
        | "iron_block"
        | "lapis_block"
        | "raw_iron_block"
        | "raw_copper_block"
        | "raw_gold_block" => return (ToolKind::Pickaxe, STONE),
        "cobweb" | "web" => return (ToolKind::Sword, 1),
        _ => {}
    }
    if n.contains("copper") && !n.contains("torch") {
        return (ToolKind::Pickaxe, STONE);
    }
    if any_contains(n, &["leaves"]) {
        return (ToolKind::Shears, 0);
    }
    if any_contains(n, &["wool"]) {
        return (ToolKind::Shears, 0);
    }
    if matches!(
        n,
        "dirt"
            | "coarse_dirt"
            | "grass_block"
            | "grass"
            | "grass_path"
            | "dirt_path"
            | "sand"
            | "red_sand"
            | "gravel"
            | "clay"
            | "soul_sand"
            | "soul_soil"
            | "snow"
            | "snow_layer"
            | "mycelium"
            | "podzol"
            | "farmland"
            | "mud"
            | "rooted_dirt"
            | "suspicious_sand"
            | "suspicious_gravel"
            | "muddy_mangrove_roots"
            | "powder_snow"
    ) || n.ends_with("concrete_powder")
    {
        return (ToolKind::Shovel, 0);
    }
    if matches!(
        n,
        "hay_block"
            | "sponge"
            | "wet_sponge"
            | "target"
            | "nether_wart_block"
            | "warped_wart_block"
            | "shroomlight"
            | "moss_block"
            | "dried_kelp_block"
            | "sculk"
            | "sculk_catalyst"
            | "sculk_sensor"
            | "sculk_shrieker"
            | "sculk_vein"
            | "pale_moss_block"
    ) {
        return (ToolKind::Hoe, 0);
    }
    let woodish = WOOD_NAMES.iter().any(|w| n.contains(w))
        && !n.contains("nether_brick")
        && !n.ends_with("_button")
        && !n.contains("nylium");
    if woodish
        || any_suffix(n, &["_log", "_wood", "_planks", "_stem", "_hyphae"])
        || matches!(
            n,
            "crafting_table"
                | "chest"
                | "trapped_chest"
                | "barrel"
                | "bookshelf"
                | "chiseled_bookshelf"
                | "ladder"
                | "noteblock"
                | "jukebox"
                | "pumpkin"
                | "carved_pumpkin"
                | "lit_pumpkin"
                | "melon_block"
                | "campfire"
                | "soul_campfire"
                | "loom"
                | "composter"
                | "lectern"
                | "cartography_table"
                | "fletching_table"
                | "smithing_table"
                | "beehive"
                | "bee_nest"
                | "daylight_detector"
                | "daylight_detector_inverted"
                | "fence"
                | "fence_gate"
                | "trapdoor"
                | "cocoa"
                | "bamboo"
                | "mangrove_roots"
        )
    {
        return (ToolKind::Axe, 0);
    }
    let stoneish = any_contains(
        n,
        &[
            "stone",
            "cobble",
            "brick",
            "deepslate",
            "andesite",
            "diorite",
            "granite",
            "_ore",
            "concrete",
            "terracotta",
            "netherrack",
            "basalt",
            "blackstone",
            "prismarine",
            "purpur",
            "quartz",
            "sandstone",
            "amethyst",
            "calcite",
            "tuff",
            "dripstone",
            "obsidian",
            "_block",
            "furnace",
            "smoker",
            "anvil",
            "cauldron",
            "hopper",
            "rail",
            "ice",
            "end_stone",
            "magma",
            "observer",
            "dispenser",
            "dropper",
            "brewing_stand",
            "bell",
            "lantern",
            "chain",
            "iron_bars",
            "iron_door",
            "iron_trapdoor",
            "nylium",
            "glazed",
            "shulker",
            "spawner",
            "conduit",
            "enchanting_table",
            "lodestone",
            "grindstone",
            "piston",
            "crafter",
            "vault",
            "trial_spawner",
            "heavy_core",
            "resin_brick",
            "creaking_heart",
        ],
    );
    if stoneish && !n.contains("glass") && !n.contains("_wart_") && !n.contains("slime") {
        let needs = !matches!(
            n,
            "ice"
                | "packed_ice"
                | "blue_ice"
                | "frosted_ice"
                | "glowstone"
                | "sea_lantern"
                | "honeycomb_block"
                | "slime"
                | "slime_block"
                | "bone_block"
                | "piston"
                | "sticky_piston"
                | "piston_arm_collision"
                | "sticky_piston_arm_collision"
                | "magma_block"
                | "shulker_box"
                | "undyed_shulker_box"
                | "melon_block"
                | "hay_block"
                | "creaking_heart"
                | "honey_block"
        ) && !n.ends_with("shulker_box")
            && !n.starts_with("infested_");
        return (ToolKind::Pickaxe, if needs { WOOD } else { 0 });
    }
    (ToolKind::None, 0)
}

/// Refines a name-derived shape using the concrete block-state compound.
pub fn shape_for_state(name: &str, base: Shape, states: Option<&NbtValue>) -> Shape {
    let Some(states) = states else {
        return base;
    };
    let n = name.strip_prefix("minecraft:").unwrap_or(name);
    let int = |key: &str| -> Option<i32> {
        match states.get(key)? {
            NbtValue::Byte(v) => Some(*v as i32),
            NbtValue::Int(v) => Some(*v),
            NbtValue::Short(v) => Some(*v as i32),
            _ => None,
        }
    };
    let string = |key: &str| states.get(key).and_then(NbtValue::as_str);

    if n.ends_with("_slab") && !n.contains("double") {
        let top =
            string("minecraft:vertical_half") == Some("top") || int("top_slot_bit") == Some(1);
        return if top {
            Shape::Box { lo: 8, hi: 16 }
        } else {
            Shape::Box { lo: 0, hi: 8 }
        };
    }
    if let Shape::Stairs { .. } = base {
        return Shape::Stairs {
            dir: int("weirdo_direction").unwrap_or(0).clamp(0, 3) as u8,
            upside_down: int("upside_down_bit") == Some(1),
        };
    }
    if n == "snow_layer" {
        let h = int("height").unwrap_or(0).clamp(0, 7) as u8;
        return if h == 0 {
            Shape::Empty
        } else {
            Shape::Box { lo: 0, hi: h * 2 }
        };
    }
    let open = int("open_bit") == Some(1);
    if n.ends_with("fence_gate") {
        return if open {
            Shape::Empty
        } else {
            Shape::Box { lo: 0, hi: 24 }
        };
    }
    if n.ends_with("_door") || n == "wooden_door" || n == "iron_door" {
        return if open { Shape::Empty } else { Shape::FULL };
    }
    if n.ends_with("trapdoor") {
        if open {
            return Shape::Empty;
        }
        return if int("upside_down_bit") == Some(1) {
            Shape::Box { lo: 13, hi: 16 }
        } else {
            Shape::Box { lo: 0, hi: 3 }
        };
    }
    base
}

/// Liquid depth (`liquid_depth` state): 0 = source, 1..=7 flowing, 8+ falling.
pub fn liquid_depth(states: Option<&NbtValue>) -> u8 {
    match states.and_then(|s| s.get("liquid_depth")) {
        Some(NbtValue::Int(v)) => (*v).clamp(0, 15) as u8,
        Some(NbtValue::Byte(v)) => (*v).clamp(0, 15) as u8,
        _ => 0,
    }
}

/// Inputs to [`dig_ticks`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DigContext {
    /// Tool that mines the block fastest.
    pub tool: Option<Tool>,
    /// Haste effect level (0 = none).
    pub haste: u8,
    /// Mining fatigue effect level (0 = none).
    pub mining_fatigue: u8,
    /// Head in water without Aqua Affinity (5x slower).
    pub in_water_without_aqua_affinity: bool,
    /// Standing on the ground (5x slower when not).
    pub on_ground: bool,
}

impl Default for DigContext {
    fn default() -> Self {
        Self {
            tool: None,
            haste: 0,
            mining_fatigue: 0,
            in_water_without_aqua_affinity: false,
            on_ground: true,
        }
    }
}

/// Returns true if the tool is effective against the given material.
pub fn tool_matches(material: &Material, tool: Option<Tool>) -> bool {
    match tool {
        Some(t) => {
            t.kind == material.tool
                || (t.kind == ToolKind::Sword && material.tool == ToolKind::Shears)
        }
        None => false,
    }
}

/// Whether the block drops its item when mined with `tool`.
pub fn can_harvest(material: &Material, tool: Option<Tool>) -> bool {
    if material.harvest == 0 {
        return true;
    }
    match tool {
        Some(t) if t.kind == material.tool => t.tier.level() + 1 >= material.harvest,
        Some(t) if material.tool == ToolKind::Sword && t.kind == ToolKind::Shears => true,
        _ => false,
    }
}

/// Number of 50 ms ticks needed to break a block, following the vanilla
/// formula. Returns `None` for unbreakable blocks.
pub fn dig_ticks(hardness: f32, material: &Material, ctx: &DigContext) -> Option<u32> {
    if hardness < 0.0 || material.flags.contains(BlockFlags::UNBREAKABLE) {
        return None;
    }
    if hardness == 0.0 {
        return Some(0);
    }
    let effective = tool_matches(material, ctx.tool);
    let mut speed = 1.0f32;
    if let (true, Some(tool)) = (effective, ctx.tool) {
        speed = match tool.kind {
            ToolKind::Shears => {
                if material.tool == ToolKind::Shears {
                    if material.flags.contains(BlockFlags::SLOW) {
                        15.0
                    } else {
                        5.0
                    }
                } else {
                    1.0
                }
            }
            ToolKind::Sword => {
                if material.tool == ToolKind::Sword {
                    15.0
                } else {
                    1.5
                }
            }
            _ => tool.tier.speed(),
        };
        if tool.efficiency > 0 {
            speed += (tool.efficiency as f32).powi(2) + 1.0;
        }
    }
    if ctx.haste > 0 {
        speed *= 1.0 + 0.2 * ctx.haste as f32;
    }
    if ctx.mining_fatigue > 0 {
        speed *= 0.3f32.powi(ctx.mining_fatigue.min(4) as i32);
    }
    if ctx.in_water_without_aqua_affinity {
        speed /= 5.0;
    }
    if !ctx.on_ground {
        speed /= 5.0;
    }
    let divisor = if can_harvest(material, ctx.tool) {
        30.0
    } else {
        100.0
    };
    let damage = speed / hardness / divisor;
    if damage >= 1.0 {
        return Some(0);
    }
    Some((1.0 / damage).ceil() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_table_lookup() {
        let stone = block_info("minecraft:stone").unwrap();
        assert_eq!(stone.hardness, 1.5);
        assert!((block_info("ice").unwrap().friction - 0.98).abs() < 1e-3);
        assert!(block_info("minecraft:not_a_block").is_none());
    }

    #[test]
    fn classification_basics() {
        assert!(classify("minecraft:air").flags.contains(BlockFlags::AIR));
        assert!(!classify("minecraft:air").flags.contains(BlockFlags::SOLID));
        assert!(classify("minecraft:stone").shape.is_full());
        assert!(classify("minecraft:water")
            .flags
            .contains(BlockFlags::WATER));
        assert!(classify("minecraft:ladder")
            .flags
            .contains(BlockFlags::CLIMBABLE));
        assert_eq!(classify("minecraft:oak_fence").shape.top(), 1.5);
        assert!(classify("minecraft:torch").shape.is_empty());
        assert!(classify("minecraft:poppy").shape.is_empty());
        assert_eq!(classify("minecraft:oak_log").tool, ToolKind::Axe);
        assert_eq!(classify("minecraft:dirt").tool, ToolKind::Shovel);
        assert_eq!(classify("minecraft:iron_ore").tool, ToolKind::Pickaxe);
        assert_eq!(classify("minecraft:diamond_ore").harvest, 3);
        assert!(classify("minecraft:bedrock")
            .flags
            .contains(BlockFlags::UNBREAKABLE));
    }

    #[test]
    fn dig_timing_matches_vanilla() {
        let stone = classify("minecraft:stone");
        // Hand on stone: 1.5 hardness, no harvest => 1/(1/1.5/100) = 150 ticks.
        assert_eq!(dig_ticks(1.5, &stone, &DigContext::default()), Some(150));
        // Wooden pickaxe: 2/1.5/30 => 22.5 -> 23 ticks.
        let ctx = DigContext {
            tool: Tool::from_item_name("minecraft:wooden_pickaxe"),
            ..Default::default()
        };
        assert_eq!(dig_ticks(1.5, &stone, &ctx), Some(23));
        // Dirt by hand: 0.5 hardness => 15 ticks.
        let dirt = classify("minecraft:dirt");
        assert_eq!(dig_ticks(0.5, &dirt, &DigContext::default()), Some(15));
        assert_eq!(
            dig_ticks(-1.0, &classify("minecraft:bedrock"), &DigContext::default()),
            None
        );
        // Diamond ore requires iron tier.
        let ore = classify("minecraft:diamond_ore");
        assert!(!can_harvest(&ore, Tool::from_item_name("stone_pickaxe")));
        assert!(can_harvest(&ore, Tool::from_item_name("iron_pickaxe")));
    }

    #[test]
    fn state_refinement() {
        let states = NbtValue::Compound(vec![(
            "minecraft:vertical_half".into(),
            NbtValue::String("top".into()),
        )]);
        let base = classify("minecraft:oak_slab").shape;
        assert_eq!(
            shape_for_state("minecraft:oak_slab", base, Some(&states)),
            Shape::Box { lo: 8, hi: 16 }
        );
        let stairs = NbtValue::Compound(vec![
            ("weirdo_direction".into(), NbtValue::Int(2)),
            ("upside_down_bit".into(), NbtValue::Byte(0)),
        ]);
        let s = shape_for_state(
            "minecraft:oak_stairs",
            classify("minecraft:oak_stairs").shape,
            Some(&stairs),
        );
        assert_eq!(
            s,
            Shape::Stairs {
                dir: 2,
                upside_down: false
            }
        );
        assert_eq!(s.boxes().1, 2);
    }
}
