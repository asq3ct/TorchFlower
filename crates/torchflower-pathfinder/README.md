# torchflower-pathfinder

Bounded A* pathfinding over TorchFlower's sparse voxel world.

- Moves:
  - walking, including diagonals
  - jumping up one block
  - dropping down (at most `max_drop`, or any height into water)
  - sprint-parkour across 1–2 block gaps
  - swimming and climbing
  - bridging and pillaring (only when scaffold blocks are available)
  - digging through blocks, with the cost taken from the bot's best tool
- Goals: `GoalBlock`, `GoalNear`, `GoalXZ`, `GoalY` and `GoalGetToBlock`. Following an entity is handled in `torchflower-bot`, which re-plans toward a `GoalNear` around the entity's current position.
- Each search is capped by a node limit (default 4000) and a timeout (default 40 ms). This keeps peak memory for one search at roughly 200 KB, which is freed when the search returns.
- If the goal cannot be reached within the budget, the search returns a partial path toward the most promising node.
- `PathFollower` turns the path into physics controls every tick. It also tells the bot when a block needs to be dug or placed, and reports `Stuck` so the bot can re-plan.
