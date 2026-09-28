//! `ItemStackRequest` (0x93) builders.

use torchflower_protocol_core::wire::{begin_packet, end_packet, put_var_i32, put_var_u32};

use crate::inventory::{slot_type, Inventory, SlotInfo, CREATED_OUTPUT_SLOT};
use crate::item::ItemStack;
use crate::recipe::Ingredient;

/// Packet id of `ItemStackRequest`.
pub const ITEM_STACK_REQUEST_ID: u32 = 0x93;

/// A single stack request action.
#[derive(Debug, Clone, PartialEq)]
pub enum StackAction {
    Take {
        count: u8,
        src: SlotInfo,
        dst: SlotInfo,
    },
    Place {
        count: u8,
        src: SlotInfo,
        dst: SlotInfo,
    },
    Swap {
        src: SlotInfo,
        dst: SlotInfo,
    },
    Drop {
        count: u8,
        src: SlotInfo,
        randomly: bool,
    },
    Destroy {
        count: u8,
        src: SlotInfo,
    },
    Consume {
        count: u8,
        src: SlotInfo,
    },
    CraftRecipe {
        recipe_network_id: u32,
        times: u8,
    },
    CraftRecipeAuto {
        recipe_network_id: u32,
        times: u8,
        ingredients: Vec<(Ingredient, i32)>,
    },
    CraftCreative {
        creative_item_network_id: u32,
        times: u8,
    },
    CraftResultsDeprecated {
        results: Vec<ItemStack>,
        times: u8,
    },
}

fn put_slot(out: &mut Vec<u8>, s: &SlotInfo) {
    out.push(s.container);
    match s.dynamic_id {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_le_bytes());
        }
        None => out.push(0),
    }
    out.push(s.slot);
    put_var_i32(out, s.stack_id);
}

impl StackAction {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            StackAction::Take { count, src, dst } | StackAction::Place { count, src, dst } => {
                out.push(if matches!(self, StackAction::Take { .. }) {
                    0
                } else {
                    1
                });
                out.push(*count);
                put_slot(out, src);
                put_slot(out, dst);
            }
            StackAction::Swap { src, dst } => {
                out.push(2);
                put_slot(out, src);
                put_slot(out, dst);
            }
            StackAction::Drop {
                count,
                src,
                randomly,
            } => {
                out.push(3);
                out.push(*count);
                put_slot(out, src);
                out.push(*randomly as u8);
            }
            StackAction::Destroy { count, src } => {
                out.push(4);
                out.push(*count);
                put_slot(out, src);
            }
            StackAction::Consume { count, src } => {
                out.push(5);
                out.push(*count);
                put_slot(out, src);
            }
            StackAction::CraftRecipe {
                recipe_network_id,
                times,
            } => {
                out.push(12);
                put_var_u32(out, *recipe_network_id);
                out.push(*times);
            }
            StackAction::CraftRecipeAuto {
                recipe_network_id,
                times,
                ingredients,
            } => {
                out.push(13);
                put_var_u32(out, *recipe_network_id);
                out.push(*times);
                out.push(*times);
                put_var_u32(out, ingredients.len() as u32);
                for (ing, count) in ingredients {
                    ing.encode(out);
                    put_var_i32(out, *count);
                }
            }
            StackAction::CraftCreative {
                creative_item_network_id,
                times,
            } => {
                out.push(14);
                put_var_u32(out, *creative_item_network_id);
                out.push(*times);
            }
            StackAction::CraftResultsDeprecated { results, times } => {
                out.push(19);
                put_var_u32(out, results.len() as u32);
                for r in results {
                    r.write_plain(out);
                }
                out.push(*times);
            }
        }
    }
}

/// One request (a list of actions under a client request id).
#[derive(Debug, Clone, PartialEq)]
pub struct StackRequest {
    pub request_id: i32,
    pub actions: Vec<StackAction>,
}

impl StackRequest {
    /// Encodes the request body (as embedded in `PlayerAuthInput` or the
    /// standalone packet).
    pub fn encode(&self, out: &mut Vec<u8>) {
        put_var_i32(out, self.request_id);
        put_var_u32(out, self.actions.len() as u32);
        for a in &self.actions {
            a.encode(out);
        }
        put_var_u32(out, 0); // filter strings
        out.extend_from_slice(&0i32.to_le_bytes()); // filter cause: server chat public
    }

    /// Appends a framed standalone `ItemStackRequest` packet.
    pub fn encode_packet(&self, out: &mut Vec<u8>) {
        let mark = begin_packet(out, ITEM_STACK_REQUEST_ID);
        put_var_u32(out, 1);
        self.encode(out);
        end_packet(out, mark);
    }
}

/// Generates client request ids (−1, −3, −5, … like the vanilla client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestIds {
    next: i32,
}

impl Default for RequestIds {
    fn default() -> Self {
        Self { next: -1 }
    }
}

impl RequestIds {
    /// Next id.
    pub fn next_id(&mut self) -> i32 {
        let id = self.next;
        self.next = if self.next <= i32::MIN + 2 {
            -1
        } else {
            self.next - 2
        };
        id
    }
}

/// Moves `count` items from unified slot `from` to `to` (Place). Swaps if the
/// destination holds a different item.
pub fn move_items(inv: &Inventory, request_id: i32, from: u8, to: u8, count: u8) -> StackRequest {
    let src = inv.slot_info(from);
    let dst = inv.slot_info(to);
    let dst_item = inv.get(to);
    let src_item = inv.get(from);
    let action = match (src_item, dst_item) {
        (Some(a), Some(b)) if a.network_id != b.network_id || a.metadata != b.metadata => {
            StackAction::Swap { src, dst }
        }
        _ => StackAction::Place { count, src, dst },
    };
    StackRequest {
        request_id,
        actions: vec![action],
    }
}

/// Swaps two unified slots.
pub fn swap_slots(inv: &Inventory, request_id: i32, a: u8, b: u8) -> StackRequest {
    StackRequest {
        request_id,
        actions: vec![StackAction::Swap {
            src: inv.slot_info(a),
            dst: inv.slot_info(b),
        }],
    }
}

/// Drops `count` items from a unified slot into the world.
pub fn drop_items(inv: &Inventory, request_id: i32, slot: u8, count: u8) -> StackRequest {
    StackRequest {
        request_id,
        actions: vec![StackAction::Drop {
            count,
            src: inv.slot_info(slot),
            randomly: false,
        }],
    }
}

/// Transfers between the open window (`window_slot`) and a unified slot.
pub fn window_transfer(
    inv: &Inventory,
    request_id: i32,
    window_slot: u8,
    inv_slot: u8,
    count: u8,
    to_window: bool,
) -> Option<StackRequest> {
    let w = inv.window_slot_info(window_slot)?;
    let p = inv.slot_info(inv_slot);
    let (src, dst) = if to_window { (p, w) } else { (w, p) };
    Some(StackRequest {
        request_id,
        actions: vec![StackAction::Place { count, src, dst }],
    })
}

/// Slot of the created-output container.
pub fn created_output(request_id: i32) -> SlotInfo {
    SlotInfo {
        container: slot_type::CREATED_OUTPUT,
        dynamic_id: None,
        slot: CREATED_OUTPUT_SLOT,
        stack_id: request_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torchflower_protocol_core::wire::{iter_packets, WireReader};

    #[test]
    fn encodes_place_request() {
        let mut inv = Inventory::default();
        inv.apply_slot(
            0,
            2,
            ItemStack {
                network_id: 1,
                count: 3,
                stack_id: 8,
                ..Default::default()
            },
        );
        let req = move_items(&inv, -1, 2, 5, 3);
        let mut out = Vec::new();
        req.encode_packet(&mut out);
        let pkt = iter_packets(&out).next().unwrap().unwrap();
        assert_eq!(pkt.id, ITEM_STACK_REQUEST_ID);
        let mut r = WireReader::new(pkt.payload);
        assert_eq!(r.var_u32().unwrap(), 1);
        assert_eq!(r.var_i32().unwrap(), -1);
        assert_eq!(r.var_u32().unwrap(), 1);
        assert_eq!(r.u8().unwrap(), 1); // place
        assert_eq!(r.u8().unwrap(), 3);
        assert_eq!(r.u8().unwrap(), slot_type::HOTBAR_AND_INVENTORY);
        assert!(!r.bool().unwrap());
        assert_eq!(r.u8().unwrap(), 2);
        assert_eq!(r.var_i32().unwrap(), 8);
    }

    #[test]
    fn request_ids_are_negative_odd() {
        let mut ids = RequestIds::default();
        assert_eq!([ids.next_id(), ids.next_id(), ids.next_id()], [-1, -3, -5]);
    }
}
