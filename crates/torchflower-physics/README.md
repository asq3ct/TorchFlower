# torchflower-physics

Player movement simulation for TorchFlower bots, run at 20 Hz without a client.

- Swept AABB collision against block shapes, including slabs, stairs, fences, carpets and snow layers.
- Gravity of 0.08 blocks/tick² with 0.98 vertical drag.
- Ground inertia is the block's slipperiness × 0.91, so ice (0.98) and slime (0.8) behave differently.
- Step-up of up to 0.6 blocks.
- Jumping (0.42 impulse), sprint-jump boost, sprinting and sneaking.
- Sneaking stops the bot at ledges.
- Water and lava drag, ladders and vines, cobwebs, and the relevant status effects.
- Fall-distance tracking and fall damage.
- `InputTracker` builds the `PlayerAuthInput` input flags from each tick's state. It handles the edge-triggered flags such as start/stop sprinting, sneaking and jumping.

Unit tests compare walking speed, sprinting speed, jump height, free-fall velocity, step-up and fall damage against vanilla values.
