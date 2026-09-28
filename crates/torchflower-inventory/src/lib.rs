//! Bedrock inventory handling for TorchFlower bots.
//!
//! * [`Inventory`] models the hotbar (0–8), main inventory (9–35), armor
//!   (36–39), off-hand (40), cursor and the currently open container.
//! * [`request`] builds `ItemStackRequest` actions (take/place/swap/drop/craft).
//! * [`recipe`] decodes `CraftingData` into a shared [`RecipeBook`] and plans
//!   2×2 / 3×3 crafts.
//! * [`ItemRegistry`] resolves item names from the `ItemRegistry` packet.
//!
//! Registries and recipe books are de-duplicated process-wide, so many bots on
//! one server share a single copy.

#![forbid(unsafe_code)]

pub mod inventory;
pub mod item;
pub mod recipe;
pub mod request;

pub use inventory::{
    decode_container_close, decode_inventory_content, decode_inventory_slot,
    decode_item_stack_response, slot_type, window_id, ContainerOpen, Hand, Inventory, SlotInfo,
    StackResponse, Window, ARMOR_START, OFFHAND_SLOT,
};
pub use item::{ItemEntry, ItemRegistry, ItemStack, ITEM_REGISTRY_ID};
pub use recipe::{
    craft_request, plan_craft, CraftError, CraftPlan, Ingredient, Recipe, RecipeBook, RecipeShape,
    CRAFTING_DATA_ID,
};
pub use request::{
    drop_items, move_items, swap_slots, window_transfer, RequestIds, StackAction, StackRequest,
    ITEM_STACK_REQUEST_ID,
};
