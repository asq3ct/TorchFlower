//! `CraftingData` (0x34) decoding and a crafting planner.
//!
//! Only crafting-table / inventory-grid recipes (shaped and shapeless) are
//! retained; furnace, smithing, brewing and multi recipes are parsed and
//! discarded to keep the shared recipe book small.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use torchflower_protocol_core::wire::{put_string, put_var_i32, WireError, WireReader};

use crate::inventory::Inventory;
use crate::item::{shared, ItemRegistry, ItemStack};
use crate::request::{created_output, StackAction, StackRequest};

/// Packet id of `CraftingData`.
pub const CRAFTING_DATA_ID: u32 = 0x34;

/// Recipe ingredient descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingredient {
    /// Empty grid cell.
    Invalid,
    /// Network id + metadata (32767 = any).
    Default {
        /// Item network id.
        network_id: i16,
        /// Metadata (32767 = any).
        metadata: i16,
    },
    /// A Molang expression (not matched by the planner).
    MoLang {
        /// The expression.
        expression: Box<str>,
        /// Molang version.
        version: u8,
    },
    /// Any item with this tag, e.g. `minecraft:planks`.
    Tag(Box<str>),
    /// An item by name and metadata.
    Deferred {
        /// Item identifier.
        name: Box<str>,
        /// Metadata (32767 = any).
        metadata: i16,
    },
    /// An item by alias name.
    ComplexAlias(Box<str>),
}

impl Ingredient {
    fn read(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(match r.u8()? {
            0 => Ingredient::Invalid,
            1 => {
                let network_id = r.i16_le()?;
                let metadata = if network_id != 0 { r.i16_le()? } else { 0 };
                Ingredient::Default {
                    network_id,
                    metadata,
                }
            }
            2 => Ingredient::MoLang {
                expression: r.string()?.into(),
                version: r.u8()?,
            },
            3 => Ingredient::Tag(r.string()?.into()),
            4 => Ingredient::Deferred {
                name: r.string()?.into(),
                metadata: r.i16_le()?,
            },
            5 => Ingredient::ComplexAlias(r.string()?.into()),
            _ => return Err(r.err("item descriptor type")),
        })
    }

    /// Encodes the descriptor (without count).
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Ingredient::Invalid => out.push(0),
            Ingredient::Default {
                network_id,
                metadata,
            } => {
                out.push(1);
                out.extend_from_slice(&network_id.to_le_bytes());
                if *network_id != 0 {
                    out.extend_from_slice(&metadata.to_le_bytes());
                }
            }
            Ingredient::MoLang {
                expression,
                version,
            } => {
                out.push(2);
                put_string(out, expression);
                out.push(*version);
            }
            Ingredient::Tag(t) => {
                out.push(3);
                put_string(out, t);
            }
            Ingredient::Deferred { name, metadata } => {
                out.push(4);
                put_string(out, name);
                out.extend_from_slice(&metadata.to_le_bytes());
            }
            Ingredient::ComplexAlias(n) => {
                out.push(5);
                put_string(out, n);
            }
        }
    }

    /// True for the empty grid cell.
    pub fn is_empty(&self) -> bool {
        matches!(
            self,
            Ingredient::Invalid | Ingredient::Default { network_id: 0, .. }
        )
    }

    /// Whether `item` satisfies this ingredient. Item tags are resolved with
    /// name heuristics (e.g. `minecraft:planks` matches `*_planks`).
    pub fn matches(&self, item: &ItemStack, items: &ItemRegistry) -> bool {
        if item.is_empty() {
            return false;
        }
        let meta_ok = |m: i16| m == 32767 || m == -1 || m as u32 == item.metadata;
        match self {
            Ingredient::Default {
                network_id,
                metadata,
            } => *network_id as i32 == item.network_id && meta_ok(*metadata),
            Ingredient::Deferred { name, metadata } => {
                items.id(name) == Some(item.network_id) && meta_ok(*metadata)
            }
            Ingredient::ComplexAlias(name) => items.id(name) == Some(item.network_id),
            Ingredient::Tag(tag) => items
                .name(item.network_id)
                .is_some_and(|n| tag_matches(tag, n)),
            Ingredient::MoLang { .. } | Ingredient::Invalid => false,
        }
    }
}

/// Heuristic item-tag membership.
pub fn tag_matches(tag: &str, item_name: &str) -> bool {
    let tag = tag.strip_prefix("minecraft:").unwrap_or(tag);
    let name = item_name.strip_prefix("minecraft:").unwrap_or(item_name);
    match tag {
        "planks" => name.ends_with("_planks"),
        "logs" | "logs_that_burn" => {
            (name.ends_with("_log")
                || name.ends_with("_wood")
                || name.ends_with("_stem")
                || name.ends_with("_hyphae"))
                && (tag == "logs" || !(name.contains("crimson") || name.contains("warped")))
        }
        "coals" => name == "coal" || name == "charcoal",
        "wool" => name.ends_with("_wool"),
        "wooden_slabs" => {
            name.ends_with("_slab") && !name.contains("stone") && !name.contains("brick")
        }
        "stone_tool_materials" | "stone_crafting_materials" => {
            matches!(name, "cobblestone" | "blackstone" | "cobbled_deepslate")
        }
        "iron_tier_tool_materials" | "iron_tool_materials" => name == "iron_ingot",
        "diamond_tier_tool_materials" | "diamond_tool_materials" => name == "diamond",
        "gold_tier_tool_materials" | "golden_tool_materials" => name == "gold_ingot",
        "copper_tier_tool_materials" | "copper_tool_materials" => name == "copper_ingot",
        "wooden_tier_tool_materials" | "wooden_tool_materials" => name.ends_with("_planks"),
        other => {
            let stem = other.trim_end_matches('s');
            !stem.is_empty() && name.contains(stem)
        }
    }
}

/// Recipe grid layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeShape {
    /// Ingredients must be arranged in a `width` × `height` pattern.
    Shaped {
        /// Pattern width (1–3).
        width: u8,
        /// Pattern height (1–3).
        height: u8,
    },
    /// Ingredients may be anywhere in the grid.
    Shapeless,
}

/// A crafting recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    /// Network id used in craft requests.
    pub network_id: u32,
    /// Recipe identifier.
    pub id: Box<str>,
    /// Grid layout.
    pub shape: RecipeShape,
    /// Non-empty ingredients with their counts.
    pub inputs: Box<[(Ingredient, i32)]>,
    /// All input cells as sent (shaped recipes include empty cells).
    pub grid: Box<[(Ingredient, i32)]>,
    /// The crafted item (count per craft).
    pub output: ItemStack,
}

impl Recipe {
    /// True if the recipe fits in the 2×2 inventory grid.
    pub fn fits_inventory_grid(&self) -> bool {
        match self.shape {
            RecipeShape::Shaped { width, height } => width <= 2 && height <= 2,
            RecipeShape::Shapeless => self.inputs.len() <= 4,
        }
    }
}

/// Shared set of crafting recipes.
#[derive(Debug, Default)]
pub struct RecipeBook {
    recipes: Vec<Recipe>,
    by_output: HashMap<i32, Vec<u32>>,
}

static RECIPE_CACHE: OnceLock<std::sync::Mutex<HashMap<u64, std::sync::Weak<RecipeBook>>>> =
    OnceLock::new();

fn read_ingredient_count(r: &mut WireReader<'_>) -> Result<(Ingredient, i32), WireError> {
    let ing = Ingredient::read(r)?;
    let count = r.var_i32()?;
    Ok((ing, count))
}

fn skip_unlock_requirement(r: &mut WireReader<'_>) -> Result<(), WireError> {
    if r.u8()? == 0 {
        let n = r.var_u32()?;
        for _ in 0..n {
            read_ingredient_count(r)?;
        }
    }
    Ok(())
}

fn read_outputs(r: &mut WireReader<'_>) -> Result<ItemStack, WireError> {
    let n = r.var_u32()?;
    let mut first = ItemStack::default();
    for i in 0..n {
        let item = ItemStack::read_plain(r)?;
        if i == 0 {
            first = item;
        }
    }
    Ok(first)
}

impl RecipeBook {
    /// Builds a book from recipes.
    pub fn from_recipes(recipes: Vec<Recipe>) -> Self {
        let mut book = RecipeBook::default();
        for r in recipes {
            book.push(r);
        }
        book
    }

    fn push(&mut self, recipe: Recipe) {
        let idx = self.recipes.len() as u32;
        self.by_output
            .entry(recipe.output.network_id)
            .or_default()
            .push(idx);
        self.recipes.push(recipe);
    }

    /// Decodes a `CraftingData` payload.
    pub fn decode(payload: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        let count = r.var_u32()?;
        let mut book = RecipeBook::default();
        for _ in 0..count {
            let kind = r.var_i32()?;
            match kind {
                0 | 5 | 6 => {
                    let id = r.string()?;
                    let n = r.var_u32()? as usize;
                    let mut inputs = Vec::with_capacity(n.min(16));
                    for _ in 0..n {
                        inputs.push(read_ingredient_count(&mut r)?);
                    }
                    let output = read_outputs(&mut r)?;
                    r.skip(16, "uuid")?;
                    let block = r.string()?;
                    r.var_i32()?;
                    skip_unlock_requirement(&mut r)?;
                    let network_id = r.var_u32()?;
                    if kind == 0 && block == "crafting_table" {
                        let inputs: Box<[_]> = inputs.into();
                        book.push(Recipe {
                            network_id,
                            id: id.into(),
                            shape: RecipeShape::Shapeless,
                            grid: inputs.clone(),
                            inputs,
                            output,
                        });
                    }
                }
                1 | 7 => {
                    let id = r.string()?;
                    let w = r.var_i32()?;
                    let h = r.var_i32()?;
                    if !(0..=3).contains(&w) || !(0..=3).contains(&h) {
                        return Err(r.err("shaped recipe size"));
                    }
                    let mut grid = Vec::with_capacity((w * h) as usize);
                    for _ in 0..w * h {
                        grid.push(read_ingredient_count(&mut r)?);
                    }
                    let output = read_outputs(&mut r)?;
                    r.skip(16, "uuid")?;
                    let block = r.string()?;
                    r.var_i32()?;
                    r.bool()?;
                    skip_unlock_requirement(&mut r)?;
                    let network_id = r.var_u32()?;
                    if kind == 1 && block == "crafting_table" {
                        let inputs: Vec<_> = grid
                            .iter()
                            .filter(|(i, _)| !i.is_empty())
                            .cloned()
                            .collect();
                        book.push(Recipe {
                            network_id,
                            id: id.into(),
                            shape: RecipeShape::Shaped {
                                width: w as u8,
                                height: h as u8,
                            },
                            inputs: inputs.into(),
                            grid: grid.into(),
                            output,
                        });
                    }
                }
                2 => {
                    r.var_i32()?;
                    ItemStack::read_plain(&mut r)?;
                    r.string()?;
                }
                3 => {
                    r.var_i32()?;
                    r.var_i32()?;
                    ItemStack::read_plain(&mut r)?;
                    r.string()?;
                }
                4 => {
                    r.skip(16, "uuid")?;
                    r.var_u32()?;
                }
                8 | 9 => {
                    r.string()?;
                    for _ in 0..3 {
                        read_ingredient_count(&mut r)?;
                    }
                    if kind == 8 {
                        ItemStack::read_plain(&mut r)?;
                    }
                    r.string()?;
                    r.var_u32()?;
                }
                _ => return Err(r.err("recipe type")),
            }
        }
        // Potion / container-change recipes and material reducers follow;
        // they are not needed for crafting and are left unparsed.
        Ok(book)
    }

    /// Decodes with process-wide de-duplication.
    pub fn decode_shared(payload: &[u8]) -> Result<Arc<Self>, WireError> {
        shared(&RECIPE_CACHE, payload, Self::decode)
    }

    /// Number of recipes.
    pub fn len(&self) -> usize {
        self.recipes.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }

    /// Recipes producing `network_id`.
    pub fn recipes_for(&self, network_id: i32) -> impl Iterator<Item = &Recipe> {
        self.by_output
            .get(&network_id)
            .into_iter()
            .flatten()
            .map(|i| &self.recipes[*i as usize])
    }
}

/// A resolved crafting plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CraftPlan {
    /// The recipe to craft.
    pub recipe: Recipe,
    /// How many times to craft it.
    pub times: u8,
    /// `(unified slot, count)` to consume.
    pub consumes: Vec<(u8, u8)>,
}

/// Why no plan could be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftError {
    /// The output item is not known.
    UnknownItem,
    /// No recipe produces the item.
    NoRecipe,
    /// The recipe needs a 3×3 grid.
    NeedsCraftingTable,
    /// The inventory lacks ingredients.
    MissingIngredients,
}

/// Finds a recipe for `output` and assigns ingredients from the inventory.
pub fn plan_craft(
    book: &RecipeBook,
    items: &ItemRegistry,
    inv: &Inventory,
    output: i32,
    times: u8,
    crafting_table: bool,
) -> Result<CraftPlan, CraftError> {
    let mut saw_recipe = false;
    let mut needs_table = false;
    for recipe in book.recipes_for(output) {
        saw_recipe = true;
        if !crafting_table && !recipe.fits_inventory_grid() {
            needs_table = true;
            continue;
        }
        let mut remaining: Vec<u16> = (0..36u8)
            .map(|s| inv.get(s).map(|i| i.count).unwrap_or(0))
            .collect();
        let mut consumes: Vec<(u8, u8)> = Vec::new();
        let mut ok = true;
        'ingredients: for (ing, count) in recipe.inputs.iter() {
            let mut need = (*count).max(1) as u32 * times as u32;
            for slot in 0..36u8 {
                if need == 0 {
                    break;
                }
                let Some(item) = inv.get(slot) else { continue };
                if remaining[slot as usize] == 0 || !ing.matches(item, items) {
                    continue;
                }
                let take = need.min(remaining[slot as usize] as u32);
                remaining[slot as usize] -= take as u16;
                need -= take;
                match consumes.iter_mut().find(|c| c.0 == slot) {
                    Some(c) => c.1 += take as u8,
                    None => consumes.push((slot, take as u8)),
                }
            }
            if need > 0 {
                ok = false;
                break 'ingredients;
            }
        }
        if ok {
            return Ok(CraftPlan {
                recipe: recipe.clone(),
                times,
                consumes,
            });
        }
    }
    Err(if !saw_recipe {
        CraftError::NoRecipe
    } else if needs_table {
        CraftError::NeedsCraftingTable
    } else {
        CraftError::MissingIngredients
    })
}

/// Builds the "recipe book" style auto-craft request for a plan, placing the
/// result into `dst_slot` (unified slot).
pub fn craft_request(
    plan: &CraftPlan,
    inv: &Inventory,
    request_id: i32,
    dst_slot: u8,
) -> StackRequest {
    let mut result = plan.recipe.output.clone();
    let total = (result.count as u32 * plan.times as u32).min(255) as u8;
    result.count = total as u16;
    let mut actions = vec![
        StackAction::CraftRecipeAuto {
            recipe_network_id: plan.recipe.network_id,
            times: plan.times,
            ingredients: plan.recipe.inputs.to_vec(),
        },
        StackAction::CraftResultsDeprecated {
            results: vec![result],
            times: plan.times,
        },
    ];
    for (slot, count) in &plan.consumes {
        actions.push(StackAction::Consume {
            count: *count,
            src: inv.slot_info(*slot),
        });
    }
    let mut dst = inv.slot_info(dst_slot);
    if inv.get(dst_slot).is_none() {
        dst.stack_id = 0;
    }
    actions.push(StackAction::Place {
        count: total,
        src: created_output(request_id),
        dst,
    });
    StackRequest {
        request_id,
        actions,
    }
}

/// Encodes a shaped/shapeless recipe back into `CraftingData` form (tests).
#[doc(hidden)]
pub fn encode_recipe_for_tests(recipe: &Recipe, out: &mut Vec<u8>) {
    use torchflower_protocol_core::wire::put_var_u32;
    match recipe.shape {
        RecipeShape::Shapeless => {
            put_var_i32(out, 0);
            put_string(out, &recipe.id);
            put_var_u32(out, recipe.grid.len() as u32);
            for (i, c) in recipe.grid.iter() {
                i.encode(out);
                put_var_i32(out, *c);
            }
        }
        RecipeShape::Shaped { width, height } => {
            put_var_i32(out, 1);
            put_string(out, &recipe.id);
            put_var_i32(out, width as i32);
            put_var_i32(out, height as i32);
            for (i, c) in recipe.grid.iter() {
                i.encode(out);
                put_var_i32(out, *c);
            }
        }
    }
    put_var_u32(out, 1);
    recipe.output.write_plain(out);
    out.extend_from_slice(&[0u8; 16]);
    put_string(out, "crafting_table");
    put_var_i32(out, 0);
    if matches!(recipe.shape, RecipeShape::Shaped { .. }) {
        out.push(1);
    }
    out.push(1); // unlock: always unlocked
    put_var_u32(out, recipe.network_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::window_id;
    use torchflower_protocol_core::wire::put_var_u32;

    fn planks_to_sticks() -> Recipe {
        let planks = (Ingredient::Tag("minecraft:planks".into()), 1);
        Recipe {
            network_id: 42,
            id: "minecraft:stick".into(),
            shape: RecipeShape::Shaped {
                width: 1,
                height: 2,
            },
            inputs: vec![planks.clone(), planks.clone()].into(),
            grid: vec![planks.clone(), planks].into(),
            output: ItemStack {
                network_id: 320,
                count: 4,
                ..Default::default()
            },
        }
    }

    fn setup() -> (ItemRegistry, Inventory) {
        let items = ItemRegistry::from_entries(vec![
            ("minecraft:oak_planks".to_string(), 5),
            ("minecraft:stick".to_string(), 320),
        ]);
        let mut inv = Inventory::default();
        let mut content = vec![ItemStack::default(); 36];
        content[2] = ItemStack {
            network_id: 5,
            count: 3,
            stack_id: 11,
            ..Default::default()
        };
        inv.apply_content(window_id::INVENTORY, content);
        (items, inv)
    }

    #[test]
    fn crafting_data_decode_keeps_crafting_recipes() {
        let mut p = Vec::new();
        put_var_u32(&mut p, 2);
        encode_recipe_for_tests(&planks_to_sticks(), &mut p);
        // A furnace recipe to be skipped.
        put_var_i32(&mut p, 2);
        put_var_i32(&mut p, 15);
        ItemStack {
            network_id: 16,
            count: 1,
            ..Default::default()
        }
        .write_plain(&mut p);
        put_string(&mut p, "furnace");
        let book = RecipeBook::decode(&p).unwrap();
        assert_eq!(book.len(), 1);
        let r = book.recipes_for(320).next().unwrap();
        assert_eq!(r.network_id, 42);
        assert!(r.fits_inventory_grid());
    }

    #[test]
    fn plans_and_builds_request() {
        let (items, inv) = setup();
        let book = RecipeBook::from_recipes(vec![planks_to_sticks()]);
        let plan = plan_craft(&book, &items, &inv, 320, 1, false).unwrap();
        assert_eq!(plan.consumes, vec![(2, 2)]);
        let req = craft_request(&plan, &inv, -3, 0);
        assert!(matches!(
            req.actions[0],
            StackAction::CraftRecipeAuto {
                recipe_network_id: 42,
                ..
            }
        ));
        assert!(matches!(
            req.actions.last(),
            Some(StackAction::Place { count: 4, .. })
        ));
        assert_eq!(
            plan_craft(&book, &items, &inv, 320, 2, false).unwrap_err(),
            CraftError::MissingIngredients
        );
        assert_eq!(
            plan_craft(&book, &items, &inv, 999, 1, false).unwrap_err(),
            CraftError::NoRecipe
        );
    }

    #[test]
    fn tag_heuristics() {
        assert!(tag_matches("minecraft:planks", "minecraft:birch_planks"));
        assert!(tag_matches("minecraft:logs", "minecraft:oak_log"));
        assert!(!tag_matches(
            "minecraft:logs_that_burn",
            "minecraft:crimson_stem"
        ));
        assert!(tag_matches(
            "minecraft:stone_tool_materials",
            "minecraft:cobblestone"
        ));
    }
}
