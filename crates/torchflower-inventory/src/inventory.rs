//! Player inventory, open containers and inventory packet decoding.
//!
//! Unified slot numbering (Mineflayer style):
//! hotbar `0..=8`, main inventory `9..=35`, armor `36..=39`, off-hand `40`.

use torchflower_protocol_core::wire::{WireError, WireReader};
use torchflower_world::{can_harvest, dig_ticks, BlockRef, DigContext, Tool};

use crate::item::{ItemRegistry, ItemStack};

/// Window ids used by the player inventory.
pub mod window_id {
    pub const INVENTORY: u32 = 0;
    pub const FIRST_CONTAINER: u32 = 1;
    pub const OFFHAND: u32 = 119;
    pub const ARMOR: u32 = 120;
    pub const CREATIVE: u32 = 121;
    pub const UI: u32 = 124;
}

/// `ContainerSlotType` ids used in item stack requests/responses.
pub mod slot_type {
    pub const ARMOR: u8 = 6;
    pub const LEVEL_ENTITY: u8 = 7;
    pub const HOTBAR_AND_INVENTORY: u8 = 12;
    pub const CRAFTING_INPUT: u8 = 13;
    pub const FURNACE_FUEL: u8 = 24;
    pub const FURNACE_INGREDIENT: u8 = 25;
    pub const FURNACE_RESULT: u8 = 26;
    pub const HOTBAR: u8 = 28;
    pub const INVENTORY: u8 = 29;
    pub const SHULKER_BOX: u8 = 30;
    pub const OFFHAND: u8 = 34;
    pub const BARREL: u8 = 58;
    pub const CURSOR: u8 = 59;
    pub const CREATED_OUTPUT: u8 = 60;
    pub const DYNAMIC: u8 = 63;
}

/// `ContainerType` of an opened window (from `ContainerOpen`).
pub mod container_type {
    pub const INVENTORY: i8 = -1;
    pub const CONTAINER: i8 = 0;
    pub const WORKBENCH: i8 = 1;
    pub const FURNACE: i8 = 2;
    pub const HOPPER: i8 = 8;
    pub const BLAST_FURNACE: i8 = 27;
    pub const SMOKER: i8 = 28;
}

/// First crafting-grid slot of the UI window for the 2×2 grid.
pub const CRAFTING_SMALL_OFFSET: u8 = 28;
/// First crafting-grid slot of the UI window for the 3×3 grid.
pub const CRAFTING_BIG_OFFSET: u8 = 32;
/// Created-output slot in the UI window.
pub const CREATED_OUTPUT_SLOT: u8 = 50;

/// Hand selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hand {
    Main,
    Off,
}

/// Slot reference inside an `ItemStackRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotInfo {
    pub container: u8,
    pub dynamic_id: Option<u32>,
    pub slot: u8,
    pub stack_id: i32,
}

impl SlotInfo {
    /// Cursor slot.
    pub fn cursor(stack_id: i32) -> Self {
        Self {
            container: slot_type::CURSOR,
            dynamic_id: None,
            slot: 0,
            stack_id,
        }
    }
}

/// An open container window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: u8,
    pub kind: i8,
    pub position: [i32; 3],
    pub entity_unique_id: i64,
    pub slots: Vec<ItemStack>,
}

impl Window {
    /// Slot type used when referencing this window in stack requests.
    pub fn slot_type(&self) -> u8 {
        match self.kind {
            container_type::FURNACE | container_type::BLAST_FURNACE | container_type::SMOKER => {
                slot_type::FURNACE_INGREDIENT
            }
            _ => slot_type::LEVEL_ENTITY,
        }
    }

    /// Slot type for a specific furnace slot (0 ingredient, 1 fuel, 2 result).
    pub fn slot_type_for(&self, slot: u8) -> u8 {
        match self.kind {
            container_type::FURNACE | container_type::BLAST_FURNACE | container_type::SMOKER => {
                match slot {
                    0 => slot_type::FURNACE_INGREDIENT,
                    1 => slot_type::FURNACE_FUEL,
                    _ => slot_type::FURNACE_RESULT,
                }
            }
            _ => self.slot_type(),
        }
    }
}

/// Complete inventory state of one bot (≈ 2 KB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    slots: Vec<ItemStack>,
    cursor: ItemStack,
    selected: u8,
    window: Option<Window>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            slots: vec![ItemStack::default(); 41],
            cursor: ItemStack::default(),
            selected: 0,
            window: None,
        }
    }
}

/// Unified slot count.
pub const SLOT_COUNT: u8 = 41;
/// First armor slot.
pub const ARMOR_START: u8 = 36;
/// Off-hand slot.
pub const OFFHAND_SLOT: u8 = 40;

impl Inventory {
    /// Item at a unified slot.
    pub fn get(&self, slot: u8) -> Option<&ItemStack> {
        self.slots.get(slot as usize).filter(|i| !i.is_empty())
    }

    /// Cursor item.
    pub fn cursor(&self) -> &ItemStack {
        &self.cursor
    }

    /// Selected hotbar slot (0..=8).
    pub fn selected_hotbar(&self) -> u8 {
        self.selected
    }

    /// Sets the selected hotbar slot locally.
    pub fn set_selected_hotbar(&mut self, slot: u8) {
        self.selected = slot.min(8);
    }

    /// Item in the main hand.
    pub fn held(&self) -> Option<&ItemStack> {
        self.get(self.selected)
    }

    /// Item in the off-hand.
    pub fn offhand(&self) -> Option<&ItemStack> {
        self.get(OFFHAND_SLOT)
    }

    /// Open container window, if any.
    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }

    /// Iterates `(slot, item)` over non-empty unified slots.
    pub fn items(&self) -> impl Iterator<Item = (u8, &ItemStack)> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, i)| !i.is_empty())
            .map(|(s, i)| (s as u8, i))
    }

    /// First slot in `0..36` holding `network_id` (hotbar first).
    pub fn find(&self, network_id: i32) -> Option<u8> {
        (0..36u8).find(|s| {
            let it = &self.slots[*s as usize];
            !it.is_empty() && it.network_id == network_id
        })
    }

    /// Total count of `network_id` in the storage slots.
    pub fn count(&self, network_id: i32) -> u32 {
        self.slots[..36]
            .iter()
            .filter(|i| !i.is_empty() && i.network_id == network_id)
            .map(|i| i.count as u32)
            .sum()
    }

    /// First empty storage slot, preferring the hotbar if `hotbar_first`.
    pub fn first_empty(&self, hotbar_first: bool) -> Option<u8> {
        let order: Box<dyn Iterator<Item = u8>> = if hotbar_first {
            Box::new(0..36u8)
        } else {
            Box::new((9..36u8).chain(0..9u8))
        };
        order
            .into_iter()
            .find(|s| self.slots[*s as usize].is_empty())
    }

    /// Swaps two unified slots locally (optimistic update before the server
    /// confirms an `ItemStackRequest`).
    pub fn swap_local(&mut self, a: u8, b: u8) {
        let (a, b) = (a as usize, b as usize);
        if a < self.slots.len() && b < self.slots.len() {
            self.slots.swap(a, b);
        }
    }

    /// Stack request slot reference for a unified slot.
    pub fn slot_info(&self, slot: u8) -> SlotInfo {
        let (container, idx) = match slot {
            0..=35 => (slot_type::HOTBAR_AND_INVENTORY, slot),
            36..=39 => (slot_type::ARMOR, slot - ARMOR_START),
            _ => (slot_type::OFFHAND, 1),
        };
        SlotInfo {
            container,
            dynamic_id: None,
            slot: idx,
            stack_id: self
                .slots
                .get(slot as usize)
                .map(|i| i.stack_id)
                .unwrap_or(0),
        }
    }

    /// Stack request slot reference for a slot in the open window.
    pub fn window_slot_info(&self, slot: u8) -> Option<SlotInfo> {
        let w = self.window.as_ref()?;
        Some(SlotInfo {
            container: w.slot_type_for(slot),
            dynamic_id: None,
            slot,
            stack_id: w.slots.get(slot as usize).map(|i| i.stack_id).unwrap_or(0),
        })
    }

    /// Applies an `InventoryContent` packet.
    pub fn apply_content(&mut self, window: u32, items: Vec<ItemStack>) {
        match window {
            window_id::INVENTORY => {
                for (i, item) in items.into_iter().take(36).enumerate() {
                    self.slots[i] = item;
                }
            }
            window_id::ARMOR => {
                for (i, item) in items.into_iter().take(4).enumerate() {
                    self.slots[ARMOR_START as usize + i] = item;
                }
            }
            window_id::OFFHAND => {
                if let Some(item) = items.into_iter().next() {
                    self.slots[OFFHAND_SLOT as usize] = item;
                }
            }
            window_id::UI => {
                if let Some(item) = items.into_iter().next() {
                    self.cursor = item;
                }
            }
            id => {
                if let Some(w) = self.window.as_mut().filter(|w| w.id as u32 == id) {
                    w.slots = items;
                }
            }
        }
    }

    /// Applies an `InventorySlot` packet.
    pub fn apply_slot(&mut self, window: u32, slot: u32, item: ItemStack) {
        let s = slot as usize;
        match window {
            window_id::INVENTORY if s < 36 => self.slots[s] = item,
            window_id::ARMOR if s < 4 => self.slots[ARMOR_START as usize + s] = item,
            window_id::OFFHAND => self.slots[OFFHAND_SLOT as usize] = item,
            window_id::UI if s == 0 => self.cursor = item,
            id => {
                if let Some(w) = self.window.as_mut().filter(|w| w.id as u32 == id) {
                    if w.slots.len() <= s {
                        w.slots.resize(s + 1, ItemStack::default());
                    }
                    w.slots[s] = item;
                }
            }
        }
    }

    /// Records a newly opened container window.
    pub fn apply_container_open(&mut self, open: ContainerOpen) {
        self.window = Some(Window {
            id: open.window_id,
            kind: open.container_type,
            position: open.position,
            entity_unique_id: open.entity_unique_id,
            slots: Vec::new(),
        });
    }

    /// Clears the open window.
    pub fn apply_container_close(&mut self, window: u8) {
        if self.window.as_ref().is_some_and(|w| w.id == window) {
            self.window = None;
        }
    }

    /// Applies an accepted `ItemStackResponse` (counts and stack ids).
    pub fn apply_stack_response(&mut self, response: &StackResponse) {
        for c in &response.containers {
            for s in &c.slots {
                let target: Option<&mut ItemStack> = match c.container {
                    slot_type::HOTBAR_AND_INVENTORY | slot_type::HOTBAR | slot_type::INVENTORY => {
                        self.slots.get_mut(s.slot as usize).filter(|_| s.slot < 36)
                    }
                    slot_type::ARMOR => self.slots.get_mut(ARMOR_START as usize + s.slot as usize),
                    slot_type::OFFHAND => self.slots.get_mut(OFFHAND_SLOT as usize),
                    slot_type::CURSOR => Some(&mut self.cursor),
                    _ => self
                        .window
                        .as_mut()
                        .and_then(|w| w.slots.get_mut(s.slot as usize)),
                };
                if let Some(item) = target {
                    if s.count == 0 {
                        *item = ItemStack::default();
                    } else {
                        item.count = s.count as u16;
                        item.stack_id = s.stack_id;
                    }
                }
            }
        }
    }

    /// Picks the fastest hotbar/inventory slot for digging `block`.
    /// Returns `(slot, ticks)`; `slot` is `None` when bare hands are best.
    pub fn best_tool(
        &self,
        items: &ItemRegistry,
        block: &BlockRef<'_>,
        ctx: DigContext,
    ) -> (Option<u8>, Option<u32>) {
        let material = block.material_info();
        let hardness = block.hardness();
        let mut best = (None, dig_ticks(hardness, &material, &ctx));
        let mut best_harvest = can_harvest(&material, None);
        for slot in 0..36u8 {
            let Some(item) = self.get(slot) else { continue };
            let Some(tool) = items.name(item.network_id).and_then(Tool::from_item_name) else {
                continue;
            };
            let c = DigContext {
                tool: Some(tool),
                ..ctx
            };
            let ticks = dig_ticks(hardness, &material, &c);
            let harvest = can_harvest(&material, Some(tool));
            let better = match (ticks, best.1) {
                (Some(t), Some(b)) => {
                    (harvest && !best_harvest) || (harvest == best_harvest && t < b)
                }
                (Some(_), None) => true,
                _ => false,
            };
            if better {
                best = (Some(slot), ticks);
                best_harvest = harvest;
            }
        }
        best
    }

    /// Heap bytes owned.
    pub fn heap_bytes(&self) -> usize {
        self.slots.capacity() * std::mem::size_of::<ItemStack>()
            + self.slots.iter().map(ItemStack::heap_bytes).sum::<usize>()
            + self
                .window
                .as_ref()
                .map(|w| w.slots.capacity() * std::mem::size_of::<ItemStack>())
                .unwrap_or(0)
    }
}

/// Decoded `ContainerOpen` (0x2e).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerOpen {
    pub window_id: u8,
    pub container_type: i8,
    pub position: [i32; 3],
    pub entity_unique_id: i64,
}

impl ContainerOpen {
    /// Decodes the payload.
    pub fn decode(payload: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(payload);
        Ok(Self {
            window_id: r.u8()?,
            container_type: r.i8()?,
            position: r.ublock_pos()?,
            entity_unique_id: r.var_i64()?,
        })
    }
}

/// Decodes `ContainerClose` (0x2f): `(window_id, container_type, server_side)`.
pub fn decode_container_close(payload: &[u8]) -> Result<(u8, i8, bool), WireError> {
    let mut r = WireReader::new(payload);
    Ok((r.u8()?, r.i8()?, r.bool()?))
}

fn skip_full_container_name(r: &mut WireReader<'_>) -> Result<(u8, Option<u32>), WireError> {
    let id = r.u8()?;
    let dynamic = if r.bool()? { Some(r.u32_le()?) } else { None };
    Ok((id, dynamic))
}

/// Decodes `InventoryContent` (0x31): `(window_id, items)`.
pub fn decode_inventory_content(payload: &[u8]) -> Result<(u32, Vec<ItemStack>), WireError> {
    let mut r = WireReader::new(payload);
    let window = r.var_u32()?;
    let count = r.var_u32()? as usize;
    if count > 1024 {
        return Err(r.err("inventory content count"));
    }
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(ItemStack::read_instance(&mut r)?);
    }
    Ok((window, items))
}

/// Decodes `InventorySlot` (0x32): `(window_id, slot, item)`.
pub fn decode_inventory_slot(payload: &[u8]) -> Result<(u32, u32, ItemStack), WireError> {
    let mut r = WireReader::new(payload);
    let window = r.var_u32()?;
    let slot = r.var_u32()?;
    skip_full_container_name(&mut r)?;
    ItemStack::read_instance(&mut r)?; // storage item
    let item = ItemStack::read_instance(&mut r)?;
    Ok((window, slot, item))
}

/// Slot update in an `ItemStackResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackResponseSlot {
    pub slot: u8,
    pub count: u8,
    pub stack_id: i32,
}

/// Container update in an `ItemStackResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackResponseContainer {
    pub container: u8,
    pub dynamic_id: Option<u32>,
    pub slots: Vec<StackResponseSlot>,
}

/// One response inside `ItemStackResponse` (0x94).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackResponse {
    /// 0 = OK.
    pub status: u8,
    pub request_id: i32,
    pub containers: Vec<StackResponseContainer>,
}

/// Decodes an `ItemStackResponse` payload.
pub fn decode_item_stack_response(payload: &[u8]) -> Result<Vec<StackResponse>, WireError> {
    let mut r = WireReader::new(payload);
    let count = r.var_u32()? as usize;
    let mut out = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let status = r.u8()?;
        let request_id = r.var_i32()?;
        let mut containers = Vec::new();
        if status == 0 {
            let n = r.var_u32()? as usize;
            for _ in 0..n {
                let (container, dynamic_id) = skip_full_container_name(&mut r)?;
                let m = r.var_u32()? as usize;
                let mut slots = Vec::with_capacity(m.min(64));
                for _ in 0..m {
                    let slot = r.u8()?;
                    let _hotbar = r.u8()?;
                    let count = r.u8()?;
                    let stack_id = r.var_i32()?;
                    r.string()?;
                    r.string()?;
                    r.var_i32()?;
                    slots.push(StackResponseSlot {
                        slot,
                        count,
                        stack_id,
                    });
                }
                containers.push(StackResponseContainer {
                    container,
                    dynamic_id,
                    slots,
                });
            }
        }
        out.push(StackResponse {
            status,
            request_id,
            containers,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use torchflower_protocol_core::wire::{put_string, put_var_i32, put_var_u32};
    use torchflower_world::{BlockRegistry, RuntimeIdMode};

    fn item(id: i32, count: u16) -> ItemStack {
        ItemStack {
            network_id: id,
            count,
            stack_id: id * 10,
            ..Default::default()
        }
    }

    #[test]
    fn content_slot_and_lookup() {
        let mut inv = Inventory::default();
        let mut items = vec![ItemStack::default(); 36];
        items[3] = item(5, 10);
        items[20] = item(5, 4);
        inv.apply_content(window_id::INVENTORY, items);
        assert_eq!(inv.find(5), Some(3));
        assert_eq!(inv.count(5), 14);
        inv.apply_slot(window_id::OFFHAND, 0, item(9, 1));
        assert_eq!(inv.offhand().unwrap().network_id, 9);
        assert_eq!(inv.slot_info(40).container, slot_type::OFFHAND);
        assert_eq!(inv.slot_info(20).slot, 20);
        assert_eq!(inv.slot_info(20).stack_id, 50);
        assert_eq!(inv.first_empty(true), Some(0));
    }

    #[test]
    fn stack_response_updates_slots() {
        let mut p = Vec::new();
        put_var_u32(&mut p, 1);
        p.push(0);
        put_var_i32(&mut p, -1);
        put_var_u32(&mut p, 1);
        p.push(slot_type::HOTBAR_AND_INVENTORY);
        p.push(0);
        put_var_u32(&mut p, 1);
        p.extend_from_slice(&[4, 4, 7]);
        put_var_i32(&mut p, 99);
        put_string(&mut p, "");
        put_string(&mut p, "");
        put_var_i32(&mut p, 0);
        let resp = decode_item_stack_response(&p).unwrap();
        assert_eq!(resp[0].request_id, -1);
        let mut inv = Inventory::default();
        inv.apply_slot(0, 4, item(3, 1));
        inv.apply_stack_response(&resp[0]);
        assert_eq!(inv.get(4).unwrap().count, 7);
        assert_eq!(inv.get(4).unwrap().stack_id, 99);
    }

    #[test]
    fn best_tool_prefers_pickaxe_for_stone() {
        let reg = Arc::new(BlockRegistry::from_states(
            vec![("minecraft:stone".to_string(), None)],
            RuntimeIdMode::Sequential,
        ));
        let items = ItemRegistry::from_entries(vec![
            ("minecraft:wooden_shovel".to_string(), 10),
            ("minecraft:stone_pickaxe".to_string(), 11),
        ]);
        let mut inv = Inventory::default();
        inv.apply_slot(0, 1, item(10, 1));
        inv.apply_slot(0, 6, item(11, 1));
        let (slot, ticks) = inv.best_tool(&items, &reg.get(0), DigContext::default());
        assert_eq!(slot, Some(6));
        assert_eq!(ticks, Some(12));
    }

    #[test]
    fn container_open_decode() {
        let mut p = vec![3u8, 0];
        torchflower_protocol_core::wire::put_ublock_pos(&mut p, [1, 64, -2]);
        torchflower_protocol_core::wire::put_var_i64(&mut p, -1);
        let open = ContainerOpen::decode(&p).unwrap();
        assert_eq!(open.position, [1, 64, -2]);
        let mut inv = Inventory::default();
        inv.apply_container_open(open);
        inv.apply_slot(3, 5, item(1, 2));
        assert_eq!(inv.window().unwrap().slots[5].count, 2);
        assert_eq!(
            inv.window_slot_info(5).unwrap().container,
            slot_type::LEVEL_ENTITY
        );
        inv.apply_container_close(3);
        assert!(inv.window().is_none());
    }
}
