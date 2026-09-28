//! Public, cloneable bot handle with an async Mineflayer-style API.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use torchflower_engine::bedrock::session::create_offline_session;
use torchflower_inventory::{
    craft_request, drop_items, move_items, plan_craft, swap_slots, window_transfer, Hand,
    Inventory, StackResponse, Window,
};
use torchflower_pathfinder::Goal;
use torchflower_physics::{look_angles, Controls, Vec3};
use torchflower_world::BlockPos;

use crate::driver::{lock, Command, Driver, Reply, Shared, StackBuilder};
use crate::entity::Entity;
use crate::error::{BotError, BotResult};
use crate::state::{Auth, BotConfig, BotEvent, BotState};
use crate::transport::{EngineTransport, Transport};

/// Options for [`Bot::dig_with`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DigOptions {
    /// Walk over the block's drops after it breaks.
    pub collect_drops: bool,
}

/// A resolved block in the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Block position.
    pub pos: BlockPos,
    /// Full identifier, e.g. `minecraft:iron_ore`.
    pub name: String,
    /// Network runtime id of the block state.
    pub runtime_id: u32,
}

/// Handle to a running bot. Cheap to clone; all clones control the same bot.
#[derive(Clone)]
pub struct Bot {
    shared: Arc<Shared>,
}

/// Mineflayer-style alias.
pub type TorchFlower = Bot;

impl std::fmt::Debug for Bot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bot")
            .field("username", &self.shared.config.username)
            .finish()
    }
}

const ACTION_TIMEOUT: Duration = Duration::from_secs(120);

impl Bot {
    /// Connects to a server, logs in and waits for the spawn.
    pub async fn connect(config: BotConfig) -> BotResult<Bot> {
        let session = match &config.auth {
            Auth::Offline => create_offline_session(&config.username)
                .map_err(|e| BotError::Transport(e.to_string()))?,
            Auth::Session(s) => (**s).clone(),
        };
        let transport =
            EngineTransport::connect(&config.host, config.port, &session, config.protocol).await?;
        let bot = Self::start(transport, config, session.chain.xuid.clone());
        bot.wait_for_spawn().await?;
        Ok(bot)
    }

    /// Starts a bot over an arbitrary [`Transport`] without waiting for the
    /// spawn. Must be called inside a Tokio runtime.
    pub fn start<T: Transport>(transport: T, config: BotConfig, xuid: String) -> Bot {
        let (commands, rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(64);
        let state = BotState::new(&config, transport.protocol());
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            events,
            commands,
            config,
            xuid,
        });
        let driver = Driver::new(transport, shared.clone(), rx);
        tokio::spawn(driver.run());
        Bot { shared }
    }

    /// Waits until the bot has spawned (bounded by `spawn_timeout`).
    pub async fn wait_for_spawn(&self) -> BotResult<()> {
        let mut rx = self.shared.events.subscribe();
        if self.is_spawned() {
            return Ok(());
        }
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(BotEvent::Spawned) => return Ok(()),
                    Ok(BotEvent::Disconnected(r)) | Ok(BotEvent::Kicked(r)) => {
                        return Err(BotError::Transport(r))
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.is_spawned() {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err(BotError::Disconnected),
                }
            }
        };
        tokio::time::timeout(self.shared.config.spawn_timeout, wait)
            .await
            .map_err(|_| BotError::Timeout("spawn"))?
    }

    // ------------------------------------------------------------------
    // State queries
    // ------------------------------------------------------------------

    /// Runs `f` with read access to the full state.
    pub fn with_state<R>(&self, f: impl FnOnce(&BotState) -> R) -> R {
        f(&lock(&self.shared.state))
    }

    /// Configuration.
    pub fn config(&self) -> &BotConfig {
        &self.shared.config
    }

    /// Username.
    pub fn username(&self) -> &str {
        &self.shared.config.username
    }

    /// True once spawned.
    pub fn is_spawned(&self) -> bool {
        self.with_state(|s| s.spawned)
    }

    /// Feet position.
    pub fn position(&self) -> Vec3 {
        self.with_state(|s| s.player.pos)
    }

    /// Velocity (blocks/tick).
    pub fn velocity(&self) -> Vec3 {
        self.with_state(|s| s.player.vel)
    }

    /// `(yaw, pitch)` in degrees.
    pub fn rotation(&self) -> (f32, f32) {
        self.with_state(|s| (s.player.yaw, s.player.pitch))
    }

    /// Health (0–20).
    pub fn health(&self) -> f32 {
        self.with_state(|s| s.health)
    }

    /// Food (0–20).
    pub fn food(&self) -> f32 {
        self.with_state(|s| s.food)
    }

    /// Runtime entity id of the bot.
    pub fn runtime_id(&self) -> u64 {
        self.with_state(|s| s.runtime_id)
    }

    /// Snapshot of the inventory.
    pub fn inventory(&self) -> Inventory {
        self.with_state(|s| s.inventory.clone())
    }

    /// Name of an item network id.
    pub fn item_name(&self, network_id: i32) -> Option<String> {
        self.with_state(|s| s.items.name(network_id).map(str::to_string))
    }

    /// Network id of an item name.
    pub fn item_id(&self, name: &str) -> Option<i32> {
        self.with_state(|s| s.items.id(name))
    }

    /// Block at `pos` if its data is loaded.
    pub fn block_at(&self, pos: BlockPos) -> Option<Block> {
        self.with_state(|s| {
            s.world.block_at(pos).map(|b| Block {
                pos,
                name: b.name().to_string(),
                runtime_id: b.runtime_id(),
            })
        })
    }

    /// Nearest block named `name` within `max_distance` blocks.
    pub fn find_block(&self, name: &str, max_distance: u32) -> Option<Block> {
        self.find_blocks(name, max_distance, 1).into_iter().next()
    }

    /// Up to `limit` nearest blocks named `name`.
    pub fn find_blocks(&self, name: &str, max_distance: u32, limit: usize) -> Vec<Block> {
        self.with_state(|s| {
            let ids = s.world.registry().runtime_ids_for_name(name);
            let full = s
                .world
                .registry()
                .get(ids.first().copied().unwrap_or(0))
                .name()
                .to_string();
            s.world
                .find_blocks(s.player.block_pos(), &ids, max_distance, limit)
                .into_iter()
                .map(|pos| Block {
                    pos,
                    name: full.clone(),
                    runtime_id: s.world.runtime_id(pos).unwrap_or(0),
                })
                .collect()
        })
    }

    /// Entity by runtime id.
    pub fn entity(&self, runtime_id: u64) -> Option<Entity> {
        self.with_state(|s| s.entities.get(runtime_id).cloned())
    }

    /// Player entity by name.
    pub fn player(&self, name: &str) -> Option<Entity> {
        self.with_state(|s| s.entities.player(name).cloned())
    }

    /// All tracked entities.
    pub fn entities(&self) -> Vec<Entity> {
        self.with_state(|s| s.entities.iter().cloned().collect())
    }

    /// Nearest entity matching `filter`.
    pub fn nearest_entity(&self, filter: impl Fn(&Entity) -> bool) -> Option<Entity> {
        self.with_state(|s| s.entities.nearest(s.player.pos, filter).cloned())
    }

    /// Currently open container window.
    pub fn current_window(&self) -> Option<Window> {
        self.with_state(|s| s.inventory.window().cloned())
    }

    /// Heap bytes owned by this bot's state (excludes shared registries).
    pub fn heap_bytes(&self) -> usize {
        self.with_state(BotState::heap_bytes)
    }

    // ------------------------------------------------------------------
    // Events
    // ------------------------------------------------------------------

    /// Subscribes to bot events.
    pub fn events(&self) -> broadcast::Receiver<BotEvent> {
        self.shared.events.subscribe()
    }

    /// Runs `handler` for every chat message from another player or the
    /// server. Handler errors are reported as [`BotEvent::HandlerError`].
    pub fn on_chat<F, Fut>(&self, handler: F) -> JoinHandle<()>
    where
        F: Fn(Bot, String, String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BotResult<()>> + Send + 'static,
    {
        let bot = self.clone();
        let mut rx = self.events();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(BotEvent::Chat {
                        sender, message, ..
                    }) => {
                        let fut = handler(bot.clone(), sender, message);
                        let events = bot.shared.events.clone();
                        tokio::spawn(async move {
                            if let Err(e) = fut.await {
                                let _ = events.send(BotEvent::HandlerError(e.to_string()));
                            }
                        });
                    }
                    Ok(BotEvent::Disconnected(_)) | Err(broadcast::error::RecvError::Closed) => {
                        break
                    }
                    _ => {}
                }
            }
        })
    }

    // ------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------

    fn send(&self, cmd: Command) -> BotResult<()> {
        self.shared
            .commands
            .send(cmd)
            .map_err(|_| BotError::Disconnected)
    }

    async fn request<R>(&self, make: impl FnOnce(Reply<R>) -> Command) -> BotResult<R> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
        match tokio::time::timeout(ACTION_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(BotError::Disconnected),
            Err(_) => Err(BotError::Timeout("action")),
        }
    }

    /// Sends a chat message (messages starting with `/` run as commands).
    pub async fn chat(&self, message: impl Into<String>) -> BotResult<()> {
        let m = message.into();
        self.request(|r| Command::Chat(m, r)).await
    }

    /// Runs a slash command.
    pub async fn command(&self, command: impl Into<String>) -> BotResult<()> {
        let c = command.into();
        self.request(|r| Command::Slash(c, r)).await
    }

    /// Sets the view rotation.
    pub fn look(&self, yaw: f32, pitch: f32) -> BotResult<()> {
        self.send(Command::Look { yaw, pitch })
    }

    /// Looks at a world point.
    pub fn look_at(&self, point: Vec3) -> BotResult<()> {
        let cfg = self.shared.config.physics;
        let eye = self.with_state(|s| s.player.eye(&cfg));
        let (yaw, pitch) = look_angles(eye, point);
        self.look(yaw, pitch)
    }

    /// Sets manual movement controls (overridden while navigating).
    pub fn set_controls(&self, controls: Controls) -> BotResult<()> {
        self.send(Command::SetControls(controls))
    }

    /// Stops all movement and navigation.
    pub fn stop(&self) -> BotResult<()> {
        self.send(Command::StopNavigation)?;
        self.send(Command::SetControls(Controls::default()))
    }

    /// Breaks the block at `pos`: selects the best tool, looks at the block,
    /// runs the break timer and waits for the server's confirmation.
    pub async fn dig(&self, pos: BlockPos) -> BotResult<()> {
        self.dig_with(pos, DigOptions::default()).await
    }

    /// Like [`Bot::dig`], with options (e.g. collect the drop afterwards).
    ///
    /// With `collect_drops`, the call returns once the block is broken and
    /// the bot has walked over the items that dropped within 4 blocks of it
    /// (pickup is best effort: the dig still succeeds if nothing is
    /// collected).
    pub async fn dig_with(&self, pos: BlockPos, options: DigOptions) -> BotResult<()> {
        self.request(|r| Command::Dig {
            pos,
            collect_drops: options.collect_drops,
            reply: r,
        })
        .await
    }

    /// Aborts the current dig.
    pub fn stop_digging(&self) -> BotResult<()> {
        self.send(Command::StopDig)
    }

    /// Places the held block against `against` on `face` (0 down … 5 east)
    /// and waits for the server's confirmation.
    pub async fn place_block(&self, against: BlockPos, face: u8) -> BotResult<()> {
        self.request(|r| Command::Place {
            against,
            face,
            reply: r,
        })
        .await
    }

    /// Places the held block at `target`, choosing a solid neighbour to click.
    pub async fn place_block_at(&self, target: BlockPos) -> BotResult<()> {
        let (against, face) = self
            .with_state(|s| {
                // Prefer clicking the block below, then the sides, then above.
                const ORDER: [(u8, u8); 6] = [(0, 1), (4, 5), (5, 4), (2, 3), (3, 2), (1, 0)];
                ORDER.iter().find_map(|&(dir, face)| {
                    let n = target.neighbor(dir);
                    s.world
                        .block_at(n)
                        .filter(|b| b.is_solid())
                        .map(|_| (n, face))
                })
            })
            .ok_or(BotError::Other(
                "no solid neighbour to place against".into(),
            ))?;
        self.place_block(against, face).await
    }

    /// Activates (right-clicks) a block, e.g. a button or door.
    pub async fn activate_block(&self, pos: BlockPos) -> BotResult<()> {
        match self.open_container(pos).await {
            Ok(_) | Err(BotError::Timeout(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Selects a hotbar slot (0–8).
    pub async fn select_hotbar(&self, slot: u8) -> BotResult<()> {
        self.request(|r| Command::SelectHotbar(slot, r)).await
    }

    /// Equips an item by name into the main or off hand.
    pub async fn equip(&self, item_name: &str, hand: Hand) -> BotResult<()> {
        let network_id = self
            .item_id(item_name)
            .ok_or_else(|| BotError::ItemNotFound(item_name.to_string()))?;
        self.request(|r| Command::Equip {
            network_id,
            hand,
            reply: r,
        })
        .await
    }

    async fn stack(&self, build: StackBuilder) -> BotResult<StackResponse> {
        self.request(|r| Command::Stack { build, reply: r }).await
    }

    /// Drops `count` items from a unified slot.
    pub async fn drop(&self, slot: u8, count: u8) -> BotResult<()> {
        self.stack(Box::new(move |inv, id| {
            if inv.get(slot).is_none() {
                return Err(BotError::ItemNotFound(format!("slot {slot}")));
            }
            Ok(drop_items(inv, id, slot, count))
        }))
        .await
        .map(|_| ())
    }

    /// Swaps two unified slots.
    pub async fn swap_slots(&self, a: u8, b: u8) -> BotResult<()> {
        self.stack(Box::new(move |inv, id| Ok(swap_slots(inv, id, a, b))))
            .await
            .map(|_| ())
    }

    /// Moves `count` items between unified slots.
    pub async fn move_slot(&self, from: u8, to: u8, count: u8) -> BotResult<()> {
        self.stack(Box::new(move |inv, id| {
            Ok(move_items(inv, id, from, to, count))
        }))
        .await
        .map(|_| ())
    }

    /// Crafts `count` of `item_name`. Pass the crafting table position for
    /// 3×3 recipes; 2×2 recipes use the inventory grid.
    pub async fn craft(
        &self,
        item_name: &str,
        count: u32,
        table: Option<BlockPos>,
    ) -> BotResult<()> {
        let (items, recipes) = self.with_state(|s| (s.items.clone(), s.recipes.clone()));
        let output = items
            .id(item_name)
            .ok_or_else(|| BotError::ItemNotFound(item_name.to_string()))?;
        let per_craft = recipes
            .recipes_for(output)
            .next()
            .map(|r| r.output.count.max(1) as u32)
            .ok_or(BotError::Craft(torchflower_inventory::CraftError::NoRecipe))?;
        let times = count.div_ceil(per_craft).clamp(1, 64) as u8;
        if let Some(pos) = table {
            self.open_container(pos).await?;
        }
        let result = self
            .stack(Box::new(move |inv, id| {
                let plan = plan_craft(&recipes, &items, inv, output, times, table.is_some())
                    .map_err(BotError::Craft)?;
                let produced = plan.recipe.output.count as u32 * times as u32;
                let dst = (0..36u8)
                    .find(|s| {
                        inv.get(*s).is_some_and(|i| {
                            i.network_id == output && i.count as u32 + produced <= 64
                        })
                    })
                    .or_else(|| inv.first_empty(false))
                    .ok_or(BotError::Other("inventory is full".into()))?;
                Ok(craft_request(&plan, inv, id, dst))
            }))
            .await;
        if table.is_some() {
            let _ = self.close_container().await;
        }
        result.map(|_| ())
    }

    /// Opens the container at `pos` and returns its window.
    pub async fn open_container(&self, pos: BlockPos) -> BotResult<Window> {
        self.request(|r| Command::OpenContainer { pos, reply: r })
            .await
    }

    /// Closes the open container.
    pub async fn close_container(&self) -> BotResult<()> {
        self.request(Command::CloseContainer).await
    }

    /// Moves `count` items from inventory slot `inv_slot` into the open
    /// container's `window_slot`.
    pub async fn deposit(&self, inv_slot: u8, window_slot: u8, count: u8) -> BotResult<()> {
        self.stack(Box::new(move |inv, id| {
            window_transfer(inv, id, window_slot, inv_slot, count, true)
                .ok_or(BotError::Other("no container is open".into()))
        }))
        .await
        .map(|_| ())
    }

    /// Moves `count` items from the open container's `window_slot` into
    /// inventory slot `inv_slot`.
    pub async fn withdraw(&self, window_slot: u8, inv_slot: u8, count: u8) -> BotResult<()> {
        self.stack(Box::new(move |inv, id| {
            window_transfer(inv, id, window_slot, inv_slot, count, false)
                .ok_or(BotError::Other("no container is open".into()))
        }))
        .await
        .map(|_| ())
    }

    /// Walks to a goal using the pathfinder (bridging/digging when allowed).
    pub async fn navigate_to<G: Goal + Send + 'static>(&self, goal: G) -> BotResult<()> {
        self.request(|r| Command::Navigate {
            goal: Box::new(goal),
            reply: r,
        })
        .await
    }

    /// Follows an entity, staying within `range` blocks, until
    /// [`Bot::stop`] is called or the entity disappears.
    pub async fn follow(&self, runtime_id: u64, range: f32) -> BotResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Follow {
            runtime_id,
            range,
            reply: tx,
        })?;
        rx.await.map_err(|_| BotError::Disconnected)?
    }

    /// Attacks an entity with the held item.
    pub async fn attack(&self, runtime_id: u64) -> BotResult<()> {
        self.request(|r| Command::Attack(runtime_id, r)).await
    }

    /// Requests a respawn after death.
    pub async fn respawn(&self) -> BotResult<()> {
        self.request(Command::Respawn).await
    }

    // ------------------------------------------------------------------
    // Modal forms
    // ------------------------------------------------------------------

    /// Forms opened by the server and not yet answered, as
    /// `(form_id, json)`.
    pub fn open_forms(&self) -> Vec<(u32, String)> {
        self.with_state(|s| s.open_forms.clone())
    }

    /// Answers form `form_id` with a raw JSON response: a button index
    /// (`"0"`) for simple forms, `"true"`/`"false"` for modal forms, or an
    /// array of values for custom forms (`["name", 2, true]`).
    pub async fn submit_form(&self, form_id: u32, response_json: &str) -> BotResult<()> {
        let json = response_json.trim().to_string();
        if json.is_empty() {
            return Err(BotError::Other("empty form response".into()));
        }
        self.request(|r| Command::FormResponse {
            form_id,
            response: Some(json),
            reply: r,
        })
        .await
    }

    /// Clicks button `button_index` of a simple (menu) form.
    pub async fn click_form_button(&self, form_id: u32, button_index: u32) -> BotResult<()> {
        self.submit_form(form_id, &button_index.to_string()).await
    }

    /// Closes form `form_id` without answering it.
    pub async fn close_form(&self, form_id: u32) -> BotResult<()> {
        self.request(|r| Command::FormResponse {
            form_id,
            response: None,
            reply: r,
        })
        .await
    }

    /// Waits for the next form request whose JSON satisfies `filter`
    /// (bounded by `timeout`). Returns `(form_id, json)`.
    pub async fn wait_for_form(
        &self,
        timeout: Duration,
        filter: impl Fn(&str) -> bool,
    ) -> BotResult<(u32, String)> {
        let mut rx = self.events();
        if let Some(f) = self.open_forms().into_iter().find(|f| filter(&f.1)) {
            return Ok(f);
        }
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(BotEvent::FormRequest { form_id, data }) if filter(&data) => {
                        return Ok((form_id, data))
                    }
                    Ok(BotEvent::Disconnected(_)) | Err(broadcast::error::RecvError::Closed) => {
                        return Err(BotError::Disconnected)
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| BotError::Timeout("form request"))?
    }

    // ------------------------------------------------------------------
    // Food
    // ------------------------------------------------------------------

    /// Eats the best food in the inventory (see [`AutoEatConfig::foods`])
    /// and returns the network id of the item eaten. The previous hotbar
    /// slot is restored afterwards.
    ///
    /// [`AutoEatConfig::foods`]: crate::AutoEatConfig::foods
    pub async fn eat(&self) -> BotResult<i32> {
        self.request(|r| Command::Eat {
            network_id: None,
            reply: r,
        })
        .await
    }

    /// Eats a specific food item by name.
    pub async fn eat_item(&self, item_name: &str) -> BotResult<i32> {
        let network_id = self
            .item_id(item_name)
            .ok_or_else(|| BotError::ItemNotFound(item_name.to_string()))?;
        self.request(|r| Command::Eat {
            network_id: Some(network_id),
            reply: r,
        })
        .await
    }

    // ------------------------------------------------------------------
    // Dropped items
    // ------------------------------------------------------------------

    /// Dropped item entities within `max_distance` blocks, nearest first.
    pub fn nearby_drops(&self, max_distance: f32) -> Vec<Entity> {
        self.with_state(|s| {
            s.entities
                .drops_near(s.player.pos, max_distance as f64)
                .into_iter()
                .filter_map(|(id, ..)| s.entities.get(id).cloned())
                .collect()
        })
    }

    /// Walks over every dropped item within `max_distance` blocks, nearest
    /// first, until they are picked up (removed by `TakeItemActor` /
    /// `RemoveActor`) or skipped after ~5 s each. Returns how many were
    /// collected.
    pub async fn collect_drops(&self, max_distance: f32) -> BotResult<u32> {
        self.request(|r| Command::CollectDrops {
            max_distance,
            reply: r,
        })
        .await
    }

    /// Disconnects the bot.
    pub fn disconnect(&self) {
        let _ = self.shared.commands.send(Command::Disconnect);
    }
}
