# torchflower-inventory

Inventory model and item-stack networking for TorchFlower bots.

- Slots use unified indexing: hotbar is 0–8, main inventory is 9–35, armor is 36–39 and the off-hand is 40. The crate also tracks the cursor and the currently open container (from `ContainerOpen` and `ContainerClose`).
- Decoders for `InventoryContent`, `InventorySlot`, `ItemStackResponse`, `ItemRegistry` and `CraftingData`.
- `ItemStackRequest` builders for the take, place, swap, drop, destroy, consume, craft-recipe, auto-craft and craft-results actions.
- A crafting planner that covers both the 2×2 inventory grid and the 3×3 crafting table. It emits recipe-book style auto-craft requests.
- `Inventory::best_tool` chooses the fastest tool for a block using the vanilla dig-time formula.

Item registries and recipe books are cached process-wide and keyed by a payload hash, so bots on the same server share one copy.
