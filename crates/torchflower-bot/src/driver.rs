//! The per-bot task: owns the transport, runs the 20 Hz tick loop, applies
//! inbound packets to [`BotState`] and drives action state machines.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use torchflower_inventory::{
    decode_container_close, decode_inventory_content_for, decode_inventory_slot_for,
    decode_item_stack_response, swap_slots, ContainerOpen, Hand, Inventory, ItemRegistry,
    RecipeBook, RequestIds, StackRequest, StackResponse, Window, OFFHAND_SLOT,
};
use torchflower_pathfinder::{
    find_path, FollowAction, FollowStatus, Goal, GoalNear, PathFollower, PathOptions, PathStatus,
    WorldView,
};
use torchflower_physics::{
    input_flags, look_angles, Controls, InputTracker, Physics, PlayerState, Vec3,
};
use torchflower_protocol::{
    compat::ResourcePackResponse, ClientCacheStatusPacket, Packet, RequestChunkRadiusPacket,
    ResourcePackClientResponsePacket, SetLocalPlayerAsInitializedPacket,
};
use torchflower_protocol_core::wire::{iter_packets, WireReader};
use torchflower_world::chunk::{decode_sub_chunk_packet, encode_sub_chunk_request_for};
use torchflower_world::{
    BlockFlags, BlockPos, DigContext, LevelChunk, RuntimeIdMode, SparseWorld, SubChunk,
    SubChunkResult,
};

use crate::error::{BotError, BotResult};
use crate::protocol::{
    self as proto, block_action, play_status, text_type, AuthInput, BlockAction, Inbound, UseItem,
};
use crate::state::{shared_registry, BotConfig, BotEvent, BotState};
use crate::transport::Transport;

pub(crate) type Reply<T> = oneshot::Sender<BotResult<T>>;

/// Builder for an item stack request, run against the live inventory.
pub(crate) type StackBuilder = Box<dyn FnOnce(&Inventory, i32) -> BotResult<StackRequest> + Send>;

/// Commands from [`crate::Bot`] handles to the driver.
pub(crate) enum Command {
    Chat(String, Reply<()>),
    Slash(String, Reply<()>),
    Look {
        yaw: f32,
        pitch: f32,
    },
    SetControls(Controls),
    Dig {
        pos: BlockPos,
        collect_drops: bool,
        reply: Reply<()>,
    },
    StopDig,
    Place {
        against: BlockPos,
        face: u8,
        reply: Reply<()>,
    },
    SelectHotbar(u8, Reply<()>),
    Equip {
        network_id: i32,
        hand: Hand,
        reply: Reply<()>,
    },
    Stack {
        build: StackBuilder,
        reply: Reply<StackResponse>,
    },
    OpenContainer {
        pos: BlockPos,
        reply: Reply<Window>,
    },
    CloseContainer(Reply<()>),
    Navigate {
        goal: Box<dyn Goal + Send>,
        reply: Reply<()>,
    },
    Follow {
        runtime_id: u64,
        range: f32,
        reply: Reply<()>,
    },
    StopNavigation,
    Attack(u64, Reply<()>),
    Respawn(Reply<()>),
    /// Answer a modal form: `Some(json)` responds, `None` closes it.
    FormResponse {
        form_id: u32,
        response: Option<String>,
        reply: Reply<()>,
    },
    /// Eat the best food in the inventory (`None` = any configured food).
    Eat {
        network_id: Option<i32>,
        reply: Reply<i32>,
    },
    /// Walk over nearby dropped items.
    CollectDrops {
        max_distance: f32,
        reply: Reply<u32>,
    },
    Disconnect,
}

pub(crate) struct Shared {
    pub state: Mutex<BotState>,
    pub events: broadcast::Sender<BotEvent>,
    pub commands: mpsc::UnboundedSender<Command>,
    pub config: BotConfig,
    pub xuid: String,
}

pub(crate) fn lock(m: &Mutex<BotState>) -> MutexGuard<'_, BotState> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn reply<T>(r: Option<Reply<T>>, v: BotResult<T>) {
    if let Some(r) = r {
        let _ = r.send(v);
    }
}

struct DigTask {
    pos: BlockPos,
    /// Pick up the drop after the block breaks.
    collect_drops: bool,
    face: i32,
    started: bool,
    elapsed: u32,
    total: u32,
    finished_at: Option<u64>,
    original: u32,
    reply: Option<Reply<()>>,
}

struct PlaceTask {
    target: BlockPos,
    sent_at: u64,
    reply: Option<Reply<()>>,
}

/// Who asked for the current route.
///
/// Drop collection walks the bot around using the same navigation slot as
/// [`Bot::goto`](crate::Bot::goto). Tagging the task lets collection cancel
/// and replace *its own* route without ever silently dropping a route the
/// user is waiting on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NavOwner {
    /// Requested through a public navigation API.
    User,
    /// Created internally to step onto a dropped item.
    Collect,
}

struct NavTask {
    owner: NavOwner,
    goal: Box<dyn Goal + Send>,
    follow: Option<(u64, f32)>,
    follow_anchor: Option<Vec3>,
    follower: PathFollower,
    needs_plan: bool,
    replans: u32,
    reply: Option<Reply<()>>,
}

/// Eating state machine: select food → hold "use item" for `eat_ticks` →
/// consume → restore the previous hotbar slot.
struct EatTask {
    /// Network id of the food being eaten.
    item: i32,
    /// Hotbar slot to restore afterwards.
    previous_slot: u8,
    /// Ticks the item has been in use; `None` until use starts.
    used_for: Option<u32>,
    /// Tick at which the consume transaction was sent.
    consumed_at: Option<u64>,
    /// Food level when eating started (to detect success).
    food_before: f32,
    reply: Option<Reply<i32>>,
}

/// Drop collection: visits dropped items nearest-first.
struct CollectTask {
    max_distance: f64,
    /// Set when the run follows a dig: the broken block and the tick it
    /// broke at. While present the run waits for the drop to spawn and then
    /// fills `targets` from the entities around that block.
    origin: Option<(BlockPos, u64)>,
    /// Items still to visit.
    targets: Vec<u64>,
    /// Item being walked to and when that started.
    current: Option<(u64, u64)>,
    /// Tick at which the bot reached the current item.
    arrived_at: Option<u64>,
    collected: u32,
    started: u64,
    /// Give up (reporting what was collected) after this tick.
    deadline: u64,
    reply: Option<Reply<u32>>,
}

struct WindowWait {
    reply: Reply<Window>,
    since: u64,
    opened_at: Option<u64>,
    content_received: bool,
}

/// Deferred async work collected while the state lock is held.
enum Deferred {
    Typed(Vec<Packet>),
    Latency(i64),
    Stop(String),
}

/// Unanswered forms kept per bot; older ones are cancelled as "busy".
const MAX_OPEN_FORMS: usize = 4;
/// Largest form JSON kept (bytes); longer payloads are truncated.
const MAX_FORM_JSON: usize = 64 * 1024;

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A correction that puts the bot further than this (blocks) from its
/// current path triggers a re-plan instead of continuing the path.
const CORRECTION_REPLAN_DISTANCE: f64 = 2.5;

/// Foods that can be eaten with a full hunger bar.
const ALWAYS_EDIBLE: &[&str] = &[
    "minecraft:golden_apple",
    "minecraft:enchanted_golden_apple",
    "minecraft:chorus_fruit",
    "minecraft:honey_bottle",
    "minecraft:milk_bucket",
    "minecraft:potion",
    "minecraft:suspicious_stew",
];

/// Radius (blocks) around a broken block searched for its drop.
const DIG_COLLECT_RADIUS: f64 = 4.0;
/// Ticks to wait for a drop to appear after a block breaks.
const DROP_SPAWN_WAIT_TICKS: u64 = 10;
/// Ticks spent trying to reach and pick up one item before skipping it.
const COLLECT_TICKS_PER_ITEM: u64 = 100;
/// Ticks to stand on an item waiting for the server to hand it over
/// (vanilla pickup delay is 10 ticks for broken blocks, 40 for thrown items).
const PICKUP_WAIT_TICKS: u64 = 45;
/// Extra ticks granted to a collect run on top of the per-item budget.
const COLLECT_DEADLINE_SLACK_TICKS: u64 = 100;

const SCAFFOLD_ITEMS: &[&str] = &[
    "minecraft:dirt",
    "minecraft:cobblestone",
    "minecraft:netherrack",
    "minecraft:cobbled_deepslate",
    "minecraft:stone",
    "minecraft:oak_planks",
    "minecraft:spruce_planks",
    "minecraft:birch_planks",
    "minecraft:end_stone",
];

pub(crate) struct Driver<T: Transport> {
    transport: T,
    shared: Arc<Shared>,
    rx: mpsc::UnboundedReceiver<Command>,
    physics: Physics,
    input: InputTracker,
    tick: u64,
    manual: Controls,
    out: Vec<u8>,
    actions: Vec<BlockAction>,
    extra_flags: u128,
    ids: RequestIds,
    dig: Option<DigTask>,
    place: Option<PlaceTask>,
    nav: Option<NavTask>,
    window_wait: Option<WindowWait>,
    pending_stack: Vec<(i32, u64, Reply<StackResponse>)>,
    eat: Option<EatTask>,
    collect: Option<CollectTask>,
    /// Earliest tick at which auto-eat may try again (after a failure).
    auto_eat_not_before: u64,
    /// Whether the `StartGame` tail matched the negotiated protocol.
    start_game_parsed: bool,
    protocol: i32,
    last_delta: Vec3,
}

impl<T: Transport> Driver<T> {
    pub(crate) fn new(
        transport: T,
        shared: Arc<Shared>,
        rx: mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        let protocol = transport.protocol();
        let physics = Physics::new(shared.config.physics);
        Self {
            transport,
            shared,
            rx,
            physics,
            input: InputTracker::default(),
            tick: 0,
            manual: Controls::default(),
            out: Vec::with_capacity(512),
            actions: Vec::with_capacity(4),
            extra_flags: 0,
            ids: RequestIds::default(),
            dig: None,
            place: None,
            nav: None,
            window_wait: None,
            pending_stack: Vec::new(),
            eat: None,
            collect: None,
            auto_eat_not_before: 0,
            start_game_parsed: false,
            protocol,
            last_delta: Vec3::ZERO,
        }
    }

    /// Replaces the world with one using `mode` for block runtime ids.
    fn set_registry(&mut self, st: &mut BotState, mode: RuntimeIdMode) {
        let registry = shared_registry(
            self.shared.config.canonical_block_states.as_ref(),
            self.protocol,
            mode,
        );
        st.world = SparseWorld::new(registry, self.shared.config.window);
    }

    /// If the ids in received chunks cannot be palette indices, the mode from
    /// `StartGame` was wrong (or its tail did not parse): switch to hashed ids
    /// and drop the chunks decoded so far.
    fn verify_runtime_id_mode(&mut self, st: &mut BotState) {
        if !st.world.runtime_ids_look_hashed() {
            return;
        }
        let dimension = st.world.dimension();
        self.set_registry(st, RuntimeIdMode::Hashed);
        st.world.set_dimension(dimension);
        st.world.set_center(st.player.block_pos());
        self.emit(BotEvent::HandlerError(format!(
            "block runtime ids are hashed, not palette indices (StartGame tail parsed: {}); \
             re-reading chunks",
            self.start_game_parsed
        )));
    }

    fn emit(&self, ev: BotEvent) {
        let _ = self.shared.events.send(ev);
    }

    pub(crate) async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let reason = loop {
            tokio::select! {
                biased;
                _ = interval.tick() => {
                    if let Err(e) = self.on_tick().await {
                        break e.to_string();
                    }
                }
                cmd = self.rx.recv() => match cmd {
                    None | Some(Command::Disconnect) => break "disconnect requested".to_string(),
                    Some(c) => self.on_command(c),
                },
                batch = self.transport.recv() => match batch {
                    Ok(b) => {
                        if let Some(stop) = self.on_batch(&b).await {
                            break stop;
                        }
                    }
                    Err(e) => break e.to_string(),
                },
            }
        };
        self.shutdown(reason).await;
    }

    async fn shutdown(mut self, reason: String) {
        if let Some(d) = self.dig.take() {
            reply(d.reply, Err(BotError::Disconnected));
        }
        if let Some(p) = self.place.take() {
            reply(p.reply, Err(BotError::Disconnected));
        }
        if let Some(n) = self.nav.take() {
            reply(n.reply, Err(BotError::Disconnected));
        }
        if let Some(w) = self.window_wait.take() {
            let _ = w.reply.send(Err(BotError::Disconnected));
        }
        for (_, _, r) in self.pending_stack.drain(..) {
            let _ = r.send(Err(BotError::Disconnected));
        }
        if let Some(e) = self.eat.take() {
            reply(e.reply, Err(BotError::Disconnected));
        }
        if let Some(c) = self.collect.take() {
            reply(c.reply, Err(BotError::Disconnected));
        }
        self.rx.close();
        while let Ok(cmd) = self.rx.try_recv() {
            fail_command(cmd);
        }
        self.transport.close().await;
        self.emit(BotEvent::Disconnected(reason));
    }

    async fn flush(&mut self) -> BotResult<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let cap = self.out.capacity().clamp(256, 16 * 1024);
        let batch = std::mem::replace(&mut self.out, Vec::with_capacity(cap));
        self.transport.send(batch).await
    }

    // ------------------------------------------------------------------
    // Tick
    // ------------------------------------------------------------------

    async fn on_tick(&mut self) -> BotResult<()> {
        let shared = self.shared.clone();
        {
            let mut st = lock(&shared.state);
            if st.spawned && !st.dead {
                self.tick_spawned(&mut st);
            }
            self.expire_waits(&mut st);
        }
        self.tick += 1;
        self.flush().await
    }

    fn tick_spawned(&mut self, st: &mut BotState) {
        self.actions.clear();
        let mut controls = self.manual;
        self.maybe_auto_eat(st);
        let eating = self.update_eat(st);
        let digging = self.update_dig(st);
        self.update_collect(st);
        if !eating {
            self.update_nav(st, &mut controls);
        }
        if (digging && self.nav.is_none()) || eating {
            controls = Controls::default();
        }
        let before = st.player.pos;
        let outcome = self.physics.tick(&mut st.player, controls, &st.world);
        self.last_delta = st.player.pos - before;
        let _ = outcome;
        let feet = st.player.block_pos();
        st.world.set_center(feet);
        if self.tick.is_multiple_of(20) {
            let pos = st.player.pos;
            st.entities.prune(pos);
        }
        if self.tick.is_multiple_of(100) {
            st.world.reset_pending_requests();
        }
        if self.tick.is_multiple_of(4) {
            let reqs = st.world.take_sub_chunk_requests(48);
            if !reqs.is_empty() {
                let (cx, cz) = feet.chunk();
                let base = [cx, feet.y >> 4, cz];
                encode_sub_chunk_request_for(
                    &mut self.out,
                    self.protocol,
                    st.world.dimension(),
                    base,
                    &reqs,
                );
            }
        }

        let snap = self.input.snapshot(&st.player, &controls);
        let cfg = self.physics.cfg;
        let eye = st.player.eye(&cfg);
        let look = st.player.look_dir();
        let flags = snap.flags | std::mem::take(&mut self.extra_flags);
        proto::encode_auth_input(
            &mut self.out,
            &AuthInput {
                pitch: st.player.pitch,
                yaw: st.player.yaw,
                position: eye.to_f32(),
                move_vector: snap.move_vector,
                head_yaw: st.player.yaw,
                flags,
                tick: self.tick,
                delta: self.last_delta.to_f32(),
                camera: look.to_f32(),
                raw_move_vector: snap.raw_move_vector,
                block_actions: &self.actions,
                item_stack_request: None,
            },
        );
    }

    fn expire_waits(&mut self, st: &mut BotState) {
        let now = self.tick;
        if let Some(p) = self.place.as_ref() {
            if now.saturating_sub(p.sent_at) > 20 {
                let p = self.place.take().expect("checked");
                let placed = st
                    .world
                    .block_at(p.target)
                    .is_some_and(|b| !b.is_air() && !b.flags().contains(BlockFlags::REPLACEABLE));
                reply(
                    p.reply,
                    if placed {
                        Ok(())
                    } else {
                        Err(BotError::Rejected("block placement not confirmed".into()))
                    },
                );
            }
        }
        if let Some(w) = self.window_wait.as_ref() {
            let open_done = w
                .opened_at
                .is_some_and(|t| w.content_received || now.saturating_sub(t) > 10);
            let timed_out = now.saturating_sub(w.since) > 60;
            if open_done || timed_out {
                let w = self.window_wait.take().expect("checked");
                let res = match st.inventory.window() {
                    Some(win) if w.opened_at.is_some() => Ok(win.clone()),
                    _ => Err(BotError::Timeout("container open")),
                };
                let _ = w.reply.send(res);
            }
        }
        let mut i = 0;
        while i < self.pending_stack.len() {
            if now.saturating_sub(self.pending_stack[i].1) > 100 {
                let (_, _, r) = self.pending_stack.swap_remove(i);
                let _ = r.send(Err(BotError::Timeout("item stack response")));
            } else {
                i += 1;
            }
        }
    }

    // ------------------------------------------------------------------
    // Digging
    // ------------------------------------------------------------------

    fn swing(&mut self, st: &BotState) {
        proto::encode_swing(&mut self.out, self.protocol, st.runtime_id);
    }

    /// Returns true while a dig is in progress.
    fn update_dig(&mut self, st: &mut BotState) -> bool {
        let Some(mut d) = self.dig.take() else {
            return false;
        };
        let cfg = self.physics.cfg;
        let eye = st.player.eye(&cfg);
        let center = d.pos.center();
        let center = Vec3::new(center[0], center[1], center[2]);
        let (yaw, pitch) = look_angles(eye, center);
        st.player.yaw = yaw;
        st.player.pitch = pitch;

        if !d.started {
            let Some(block) = st.world.block_at(d.pos) else {
                reply(d.reply, Err(BotError::NotLoaded(d.pos.to_array())));
                return false;
            };
            if block.is_air() || block.is_water() || block.is_lava() {
                reply(d.reply, Err(BotError::NothingToDig));
                return false;
            }
            if block.hardness() < 0.0 {
                reply(d.reply, Err(BotError::Unbreakable));
                return false;
            }
            let dist = eye.distance(center);
            if dist > self.shared.config.reach + 0.5 {
                reply(d.reply, Err(BotError::OutOfReach(dist)));
                return false;
            }
            let dir = center - eye;
            d.face = st
                .world
                .raycast([eye.x, eye.y, eye.z], [dir.x, dir.y, dir.z], dist + 1.0)
                .filter(|h| h.pos == d.pos)
                .map(|h| h.face as i32)
                .unwrap_or(1);
            d.original = block.runtime_id();
            let ctx = DigContext {
                tool: None,
                on_ground: st.player.on_ground,
                in_water_without_aqua_affinity: st.player.in_water,
                ..DigContext::default()
            };
            let (slot, ticks) = st.inventory.best_tool(&st.items, &block, ctx);
            if let Some(slot) = slot {
                self.hold_slot(st, slot);
            }
            d.total = ticks.unwrap_or(0) + 1;
            self.actions.push(BlockAction {
                action: block_action::START_BREAK,
                pos: d.pos.to_array(),
                face: d.face,
            });
            if !st.server_authoritative_breaking {
                proto::encode_player_action(
                    &mut self.out,
                    self.protocol,
                    st.runtime_id,
                    block_action::START_BREAK,
                    d.pos.to_array(),
                    d.face,
                );
            }
            self.swing(st);
            d.started = true;
            self.dig = Some(d);
            return true;
        }

        if d.finished_at.is_none() {
            d.elapsed += 1;
            if d.elapsed >= d.total {
                self.actions.push(BlockAction {
                    action: block_action::PREDICT_DESTROY_BLOCK,
                    pos: d.pos.to_array(),
                    face: d.face,
                });
                self.actions.push(BlockAction {
                    action: block_action::STOP_BREAK,
                    pos: d.pos.to_array(),
                    face: d.face,
                });
                if !st.server_authoritative_breaking {
                    proto::encode_player_action(
                        &mut self.out,
                        self.protocol,
                        st.runtime_id,
                        block_action::STOP_BREAK,
                        d.pos.to_array(),
                        d.face,
                    );
                    let held = st.inventory.held().cloned().unwrap_or_default();
                    let rid = st.world.runtime_id(d.pos).unwrap_or(0);
                    proto::encode_use_item(
                        &mut self.out,
                        self.protocol,
                        &UseItem {
                            action: 2,
                            block_pos: d.pos.to_array(),
                            face: d.face,
                            hotbar_slot: st.inventory.selected_hotbar() as i32,
                            held: &held,
                            player_pos: eye.to_f32(),
                            click_pos: [0.0; 3],
                            block_runtime_id: rid,
                        },
                    );
                }
                d.finished_at = Some(self.tick);
            } else {
                self.actions.push(BlockAction {
                    action: block_action::CRACK_BREAK,
                    pos: d.pos.to_array(),
                    face: d.face,
                });
                if d.elapsed.is_multiple_of(5) {
                    self.swing(st);
                }
            }
            self.dig = Some(d);
            return true;
        }

        let finished = d.finished_at.unwrap_or(self.tick);
        if self.tick.saturating_sub(finished) > 20 {
            let broken = st
                .world
                .block_at(d.pos)
                .is_none_or(|b| b.is_air() || b.runtime_id() != d.original);
            if !broken {
                reply(
                    d.reply,
                    Err(BotError::Rejected("block break not confirmed".into())),
                );
                return false;
            }
            // Broken without an `UpdateBlock` to confirm it (the server
            // accepted the client's prediction silently). This is a normal
            // completion, so an auto-collect still has to run.
            if d.collect_drops {
                self.queue_drop_collection(d.pos, d.reply);
            } else {
                reply(d.reply, Ok(()));
            }
            return false;
        }
        self.dig = Some(d);
        true
    }

    /// Makes `slot` the held item (selecting it or swapping it into the
    /// selected hotbar slot).
    fn hold_slot(&mut self, st: &mut BotState, slot: u8) {
        let selected = st.inventory.selected_hotbar();
        if slot == selected {
            return;
        }
        let target = if slot < 9 {
            slot
        } else {
            let id = self.ids.next_id();
            swap_slots(&st.inventory, id, slot, selected).encode_packet(&mut self.out);
            st.inventory.swap_local(slot, selected);
            selected
        };
        st.inventory.set_selected_hotbar(target);
        let item = st.inventory.get(target).cloned().unwrap_or_default();
        proto::encode_mob_equipment(&mut self.out, self.protocol, st.runtime_id, &item, target);
    }

    // ------------------------------------------------------------------
    // Eating
    // ------------------------------------------------------------------

    /// Best food in the inventory: `(slot, network_id)`, following the
    /// configured preference order. `only` restricts it to one item.
    fn find_food(&self, st: &BotState, only: Option<i32>) -> Option<(u8, i32)> {
        if let Some(id) = only {
            return st.inventory.find(id).map(|slot| (slot, id));
        }
        self.shared
            .config
            .eat
            .foods
            .iter()
            .filter_map(|name| st.items.id(name))
            .find_map(|id| st.inventory.find(id).map(|slot| (slot, id)))
    }

    /// Starts eating. With no suitable food, replies `ItemNotFound`.
    fn start_eat(&mut self, st: &mut BotState, only: Option<i32>, r: Option<Reply<i32>>) {
        let Some((slot, item)) = self.find_food(st, only) else {
            reply(r, Err(BotError::ItemNotFound("food".into())));
            return;
        };
        // Servers refuse normal food when the hunger bar is full; only a few
        // items can always be eaten.
        let always = st
            .items
            .name(item)
            .is_some_and(|n| ALWAYS_EDIBLE.contains(&n));
        if st.food >= 20.0 && !always && st.game_mode != 1 {
            reply(r, Err(BotError::Other("not hungry (food is full)".into())));
            return;
        }
        let previous_slot = st.inventory.selected_hotbar();
        self.hold_slot(st, slot);
        self.eat = Some(EatTask {
            item,
            previous_slot,
            used_for: None,
            consumed_at: None,
            food_before: st.food,
            reply: r,
        });
    }

    fn click_air(&mut self, st: &BotState) {
        let cfg = self.physics.cfg;
        let eye = st.player.eye(&cfg);
        let held = st.inventory.held().cloned().unwrap_or_default();
        proto::encode_use_item(
            &mut self.out,
            self.protocol,
            &UseItem {
                action: proto::use_item_action::CLICK_AIR,
                block_pos: [0, 0, 0],
                face: 255,
                hotbar_slot: st.inventory.selected_hotbar() as i32,
                held: &held,
                player_pos: eye.to_f32(),
                click_pos: [0.0; 3],
                block_runtime_id: 0,
            },
        );
    }

    /// Returns true while eating (movement is paused).
    fn update_eat(&mut self, st: &mut BotState) -> bool {
        let Some(mut e) = self.eat.take() else {
            return false;
        };
        let eat_ticks = self.shared.config.eat.eat_ticks.max(1);
        // Food must still be in hand (it may have been moved or used up).
        let holding = st.inventory.held().is_some_and(|h| h.network_id == e.item);
        match (e.used_for, e.consumed_at) {
            (None, _) => {
                if !holding {
                    self.finish_eat(st, e, Err(BotError::ItemNotFound("food".into())));
                    return false;
                }
                // Tick 0: start using the item.
                self.click_air(st);
                self.extra_flags |= input_flags::START_USING_ITEM;
                e.used_for = Some(0);
            }
            (Some(n), None) => {
                if !holding {
                    self.finish_eat(st, e, Err(BotError::Cancelled));
                    return false;
                }
                let n = n + 1;
                e.used_for = Some(n);
                if n >= eat_ticks {
                    // Second click-air consumes (vanilla client behaviour,
                    // accepted by BDS, PocketMine and Dragonfly); the consume
                    // release is ignored for food by those servers.
                    self.click_air(st);
                    let cfg = self.physics.cfg;
                    let eye = st.player.eye(&cfg);
                    let held = st.inventory.held().cloned().unwrap_or_default();
                    proto::encode_release_item(
                        &mut self.out,
                        self.protocol,
                        proto::release_item_action::CONSUME,
                        st.inventory.selected_hotbar() as i32,
                        &held,
                        eye.to_f32(),
                    );
                    e.consumed_at = Some(self.tick);
                }
            }
            (Some(_), Some(at)) => {
                // Waiting for the server to confirm (ActorEvent / hunger).
                if st.food > e.food_before {
                    let item = e.item;
                    self.finish_eat(st, e, Ok(item));
                    return false;
                }
                if self.tick.saturating_sub(at) > 40 {
                    self.finish_eat(
                        st,
                        e,
                        Err(BotError::Rejected("food was not consumed".into())),
                    );
                    return false;
                }
            }
        }
        self.eat = Some(e);
        true
    }

    fn finish_eat(&mut self, st: &mut BotState, e: EatTask, result: BotResult<i32>) {
        if result.is_err() && e.reply.is_none() {
            // Auto-eat failed: back off before trying again.
            self.auto_eat_not_before = self.tick + 200;
        }
        if st.inventory.selected_hotbar() != e.previous_slot {
            self.hold_slot(st, e.previous_slot);
        }
        if let Ok(item) = result {
            self.emit(BotEvent::Ate { item });
        }
        reply(e.reply, result);
    }

    /// Starts an automatic eat when thresholds are crossed.
    fn maybe_auto_eat(&mut self, st: &mut BotState) {
        let cfg = &self.shared.config;
        if !cfg.auto_eat
            || self.eat.is_some()
            || self.dig.is_some()
            || self.place.is_some()
            || self.tick < self.auto_eat_not_before
            || st.game_mode == 1
        {
            return;
        }
        let hungry = st.food <= cfg.eat.food_at_or_below;
        let hurt = st.health < cfg.eat.health_below && st.food < 20.0;
        // Eating stops movement for ~1.6 s: only start on solid ground, and
        // not mid-route unless it is urgent (low health or very hungry).
        let urgent = st.health <= 8.0 || st.food <= 6.0;
        let safe = st.player.on_ground && !st.player.in_water && !st.player.in_lava;
        if !safe || (self.nav.is_some() && !urgent) {
            return;
        }
        if hungry || hurt {
            if self.find_food(st, None).is_some() {
                self.start_eat(st, None, None);
            } else {
                self.auto_eat_not_before = self.tick + 200;
            }
        }
    }

    // ------------------------------------------------------------------
    // Drop collection
    // ------------------------------------------------------------------

    /// After a successful dig: wait for the drop to spawn, then collect
    /// drops near the broken block and reply to the dig.
    fn queue_drop_collection(&mut self, pos: BlockPos, dig_reply: Option<Reply<()>>) {
        self.abort_collect();
        let (tx, rx) = tokio::sync::oneshot::channel::<BotResult<u32>>();
        self.collect = Some(CollectTask {
            max_distance: DIG_COLLECT_RADIUS,
            origin: Some((pos, self.tick)),
            targets: Vec::new(),
            current: None,
            arrived_at: None,
            collected: 0,
            started: self.tick,
            // Replaced once the drop-spawn wait ends.
            deadline: u64::MAX,
            reply: Some(tx),
        });
        if let Some(r) = dig_reply {
            tokio::spawn(async move {
                // The block is broken either way; drop pickup is best effort.
                let _ = rx.await;
                let _ = r.send(Ok(()));
            });
        }
    }

    /// Walks over queued drops. Returns true while collecting.
    fn update_collect(&mut self, st: &mut BotState) -> bool {
        let Some(mut c) = self.collect.take() else {
            return false;
        };
        let now = self.tick;
        if let Some((origin, broke_at)) = c.origin {
            if now.saturating_sub(broke_at) < DROP_SPAWN_WAIT_TICKS {
                self.collect = Some(c);
                return true;
            }
            c.origin = None;
            let o = origin.center();
            c.targets = st
                .entities
                .drops_near(Vec3::new(o[0], o[1], o[2]), c.max_distance)
                .into_iter()
                .map(|d| d.0)
                .collect();
            c.started = now;
            c.deadline = self.collect_deadline(c.targets.len());
        }
        if now > c.deadline {
            self.finish_collect(c);
            return false;
        }
        if let Some((target, since)) = c.current {
            let target_pos = st.entities.get(target).map(|e| e.position);
            match target_pos {
                None => {
                    // Gone: picked up (counted when `TakeItemActor` named
                    // this bot as the taker) or despawned. Either way stop
                    // walking towards it.
                    c.current = None;
                    c.arrived_at = None;
                    self.clear_collect_nav();
                }
                Some(pos) => {
                    // Vanilla pickup: the item touches the player's hitbox
                    // grown by 1 block horizontally / 0.5 vertically.
                    let d = pos - st.player.pos;
                    let close = d.horizontal_length() < 0.8 && d.y > -0.6 && d.y < 1.9;
                    if close && c.arrived_at.is_none() {
                        c.arrived_at = Some(now);
                    }
                    let waited_on_item = c
                        .arrived_at
                        .is_some_and(|t| now.saturating_sub(t) > PICKUP_WAIT_TICKS);
                    let too_long = now.saturating_sub(since) > COLLECT_TICKS_PER_ITEM;
                    let gave_up = !self.has_collect_nav() && !close;
                    if waited_on_item || too_long || gave_up {
                        // Could not pick it up (full inventory, pickup
                        // delay, unreachable): skip it.
                        c.current = None;
                        c.arrived_at = None;
                        self.clear_collect_nav();
                    }
                }
            }
        }
        if c.current.is_none() {
            let center = st.player.pos;
            let limit = c.max_distance + 4.0;
            c.targets.retain(|id| {
                st.entities
                    .get(*id)
                    .is_some_and(|e| e.position.distance(center) <= limit)
            });
            if c.targets.is_empty() {
                self.finish_collect(c);
                return false;
            }
            let next = c.targets.remove(0);
            let pos = st.entities.get(next).map(|e| e.position).unwrap_or(center);
            // Take over the route. Starting a user route aborts the run, so
            // there should be nothing to answer here, but answer it rather
            // than drop a reply if that ever changes.
            if let Some(old) = self.nav.take() {
                reply(old.reply, Err(BotError::Cancelled));
            }
            // Walk onto the item: its own block, or the block above if the
            // item rests on a partial block (slab, carpet) occupying it.
            let mut feet = BlockPos::from_f64(pos.x, pos.y + 0.05, pos.z);
            if st.world.shape_at(feet).is_some_and(|s| !s.is_empty()) {
                feet = feet.offset(0, 1, 0);
            }
            self.nav = Some(NavTask {
                owner: NavOwner::Collect,
                goal: Box::new(GoalNear::new(feet, 0)),
                follow: None,
                follow_anchor: None,
                follower: PathFollower::default(),
                needs_plan: true,
                replans: 0,
                reply: None,
            });
            c.current = Some((next, now));
        }
        self.collect = Some(c);
        true
    }

    /// Deadline for a collect run: a per-item walking budget plus slack,
    /// saturating so a long-lived bot can never overflow the tick counter.
    fn collect_deadline(&self, items: usize) -> u64 {
        self.tick.saturating_add(
            COLLECT_TICKS_PER_ITEM
                .saturating_mul(items.max(1) as u64)
                .saturating_add(COLLECT_DEADLINE_SLACK_TICKS),
        )
    }

    /// True while the active route belongs to drop collection.
    fn has_collect_nav(&self) -> bool {
        self.nav
            .as_ref()
            .is_some_and(|n| n.owner == NavOwner::Collect)
    }

    /// Drops the route created by drop collection, leaving a user route
    /// (and its pending reply) untouched.
    fn clear_collect_nav(&mut self) {
        if self.has_collect_nav() {
            self.nav = None;
        }
    }

    fn finish_collect(&mut self, c: CollectTask) {
        self.clear_collect_nav();
        reply(c.reply, Ok(c.collected));
    }

    /// Abandons an in-flight collect run, reporting what it managed to pick
    /// up. Used when the user takes over navigation or the bot dies.
    fn abort_collect(&mut self) {
        if let Some(c) = self.collect.take() {
            self.clear_collect_nav();
            reply(c.reply, Ok(c.collected));
        }
    }

    // ------------------------------------------------------------------
    // Placing
    // ------------------------------------------------------------------

    fn start_place(
        &mut self,
        st: &mut BotState,
        against: BlockPos,
        face: u8,
        r: Option<Reply<()>>,
    ) {
        if self.place.is_some() {
            reply(
                r,
                Err(BotError::Other("another placement is pending".into())),
            );
            return;
        }
        let target = against.neighbor(face);
        let Some(base) = st.world.block_at(against) else {
            reply(r, Err(BotError::NotLoaded(against.to_array())));
            return;
        };
        if base.is_air() || base.flags().contains(BlockFlags::LIQUID) {
            reply(
                r,
                Err(BotError::Other("cannot place against air or liquid".into())),
            );
            return;
        }
        let base_rid = base.runtime_id();
        match st.world.block_at(target) {
            Some(b) if b.is_air() || b.flags().contains(BlockFlags::REPLACEABLE) => {}
            Some(_) => {
                reply(r, Err(BotError::Occupied));
                return;
            }
            None => {
                reply(r, Err(BotError::NotLoaded(target.to_array())));
                return;
            }
        }
        let Some(held) = st.inventory.held().cloned() else {
            reply(r, Err(BotError::EmptyHand));
            return;
        };
        let cfg = self.physics.cfg;
        let eye = st.player.eye(&cfg);
        let n = torchflower_world::face_normal(face);
        let c = against.center();
        let point = [
            c[0] + n[0] as f64 * 0.5,
            c[1] + n[1] as f64 * 0.5,
            c[2] + n[2] as f64 * 0.5,
        ];
        let dist = eye.distance(Vec3::new(point[0], point[1], point[2]));
        if dist >= self.shared.config.reach {
            reply(r, Err(BotError::OutOfReach(dist)));
            return;
        }
        if !st
            .world
            .can_see_block([eye.x, eye.y, eye.z], against, Some(point))
        {
            reply(r, Err(BotError::NoLineOfSight));
            return;
        }
        // Do not place a solid block inside the bot's own hitbox.
        let cell = torchflower_physics::Aabb::new(
            Vec3::new(target.x as f64, target.y as f64, target.z as f64),
            Vec3::new(
                target.x as f64 + 1.0,
                target.y as f64 + 1.0,
                target.z as f64 + 1.0,
            ),
        );
        if held.block_runtime_id != 0 && cell.intersects(&st.player.aabb(&cfg)) {
            reply(r, Err(BotError::Occupied));
            return;
        }
        let (yaw, pitch) = look_angles(eye, Vec3::new(point[0], point[1], point[2]));
        st.player.yaw = yaw;
        st.player.pitch = pitch;
        let click = [
            (point[0] - against.x as f64) as f32,
            (point[1] - against.y as f64) as f32,
            (point[2] - against.z as f64) as f32,
        ];
        proto::encode_use_item(
            &mut self.out,
            self.protocol,
            &UseItem {
                action: 0,
                block_pos: against.to_array(),
                face: face as i32,
                hotbar_slot: st.inventory.selected_hotbar() as i32,
                held: &held,
                player_pos: eye.to_f32(),
                click_pos: click,
                block_runtime_id: base_rid,
            },
        );
        self.swing(st);
        self.place = Some(PlaceTask {
            target,
            sent_at: self.tick,
            reply: r,
        });
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    fn scaffold_slot(st: &BotState) -> Option<u8> {
        SCAFFOLD_ITEMS
            .iter()
            .filter_map(|n| st.items.id(n))
            .find_map(|id| st.inventory.find(id))
    }

    fn scaffold_count(st: &BotState) -> u32 {
        SCAFFOLD_ITEMS
            .iter()
            .filter_map(|n| st.items.id(n))
            .map(|id| st.inventory.count(id))
            .sum()
    }

    fn plan(&self, st: &BotState, nav: &mut NavTask) -> PathStatus {
        let items: &ItemRegistry = &st.items;
        let inv = &st.inventory;
        let world: &SparseWorld = &st.world;
        let on_ground = st.player.on_ground;
        let view = WorldView {
            world,
            dig: |p: BlockPos| {
                let b = world.block_at(p)?;
                if b.flags().contains(BlockFlags::INTERACTABLE) {
                    return None;
                }
                inv.best_tool(
                    items,
                    &b,
                    DigContext {
                        on_ground,
                        ..DigContext::default()
                    },
                )
                .1
            },
        };
        let opt = PathOptions {
            scaffold_blocks: Self::scaffold_count(st),
            ..self.shared.config.path
        };
        let result = find_path(&view, st.player.block_pos(), &nav.goal, &opt);
        let status = result.status;
        nav.follower = PathFollower::new(result.steps);
        status
    }

    fn update_nav(&mut self, st: &mut BotState, controls: &mut Controls) {
        let Some(mut nav) = self.nav.take() else {
            return;
        };
        if let Some((rid, range)) = nav.follow {
            let Some(target) = st.entities.get(rid).map(|e| e.position) else {
                reply(nav.reply, Err(BotError::EntityNotFound));
                return;
            };
            let moved = nav.follow_anchor.is_none_or(|a| a.distance(target) > 1.5);
            if moved && self.tick.is_multiple_of(10) {
                nav.goal = Box::new(GoalNear::new(
                    BlockPos::from_f64(target.x, target.y, target.z),
                    range,
                ));
                nav.follow_anchor = Some(target);
                nav.needs_plan = true;
            }
        }
        if nav.needs_plan {
            nav.needs_plan = false;
            let status = self.plan(st, &mut nav);
            if status == PathStatus::NoPath && !nav.goal.is_end(st.player.block_pos()) {
                nav.replans += 1;
                if nav.replans > 8 && nav.follow.is_none() {
                    reply(nav.reply, Err(BotError::NoPath));
                    return;
                }
            }
        }
        if self.dig.is_some() || self.place.is_some() {
            *controls = Controls::default();
            self.nav = Some(nav);
            return;
        }
        let eye_h = self.physics.cfg.eye_height;
        let out = {
            let world = &st.world;
            let view = WorldView {
                world,
                dig: |_p: BlockPos| Some(1),
            };
            nav.follower.tick(&st.player, &view, eye_h)
        };
        if let Some((yaw, pitch)) = out.look {
            st.player.yaw = yaw;
            st.player.pitch = pitch;
        }
        *controls = out.controls;
        match out.action {
            Some(FollowAction::Dig(pos)) => {
                self.dig = Some(DigTask {
                    pos,
                    collect_drops: false,
                    face: 1,
                    started: false,
                    elapsed: 0,
                    total: 0,
                    finished_at: None,
                    original: 0,
                    reply: None,
                });
            }
            Some(FollowAction::Place { against, face, .. }) => {
                if let Some(slot) = Self::scaffold_slot(st) {
                    self.hold_slot(st, slot);
                    self.start_place(st, against, face, None);
                } else {
                    nav.needs_plan = true;
                }
            }
            None => {}
        }
        match out.status {
            FollowStatus::Running => {}
            FollowStatus::Done | FollowStatus::Stuck => {
                if nav.goal.is_end(st.player.block_pos()) && out.status == FollowStatus::Done {
                    if nav.follow.is_none() {
                        reply(nav.reply, Ok(()));
                        return;
                    }
                } else {
                    nav.replans += 1;
                    if nav.replans > 12 && nav.follow.is_none() {
                        reply(nav.reply, Err(BotError::NoPath));
                        return;
                    }
                    nav.needs_plan = true;
                }
            }
        }
        self.nav = Some(nav);
    }

    // ------------------------------------------------------------------
    // Commands
    // ------------------------------------------------------------------

    fn on_command(&mut self, cmd: Command) {
        let shared = self.shared.clone();
        let mut st = lock(&shared.state);
        let spawned = st.spawned;
        if !spawned && !matches!(cmd, Command::SetControls(_) | Command::Look { .. }) {
            drop(st);
            fail_command_with(cmd, BotError::Other("bot has not spawned yet".into()));
            return;
        }
        match cmd {
            Command::Chat(msg, r) => {
                if msg.starts_with('/') {
                    self.encode_command(&msg);
                } else {
                    proto::encode_chat(
                        &mut self.out,
                        self.protocol,
                        &st.username,
                        &msg,
                        &shared.xuid,
                    );
                }
                let _ = r.send(Ok(()));
            }
            Command::Slash(c, r) => {
                self.encode_command(&c);
                let _ = r.send(Ok(()));
            }
            Command::Look { yaw, pitch } => {
                st.player.yaw = yaw;
                st.player.pitch = pitch.clamp(-90.0, 90.0);
            }
            Command::SetControls(c) => self.manual = c,
            Command::Dig {
                pos,
                collect_drops,
                reply: r,
            } => {
                if let Some(old) = self.dig.take() {
                    self.abort_dig(&old);
                    reply(old.reply, Err(BotError::Cancelled));
                }
                self.dig = Some(DigTask {
                    pos,
                    collect_drops,
                    face: 1,
                    started: false,
                    elapsed: 0,
                    total: 0,
                    finished_at: None,
                    original: 0,
                    reply: Some(r),
                });
            }
            Command::StopDig => {
                if let Some(old) = self.dig.take() {
                    self.abort_dig(&old);
                    reply(old.reply, Err(BotError::Cancelled));
                }
            }
            Command::Place {
                against,
                face,
                reply: r,
            } => self.start_place(&mut st, against, face, Some(r)),
            Command::SelectHotbar(slot, r) => {
                self.hold_slot(&mut st, slot.min(8));
                let _ = r.send(Ok(()));
            }
            Command::Equip {
                network_id,
                hand,
                reply: r,
            } => {
                let found = match hand {
                    Hand::Main
                        if st
                            .inventory
                            .held()
                            .is_some_and(|i| i.network_id == network_id) =>
                    {
                        Some(st.inventory.selected_hotbar())
                    }
                    _ => st.inventory.find(network_id),
                };
                let Some(slot) = found else {
                    let _ = r.send(Err(BotError::ItemNotFound(network_id.to_string())));
                    return;
                };
                match hand {
                    Hand::Main => self.hold_slot(&mut st, slot),
                    Hand::Off => {
                        let id = self.ids.next_id();
                        swap_slots(&st.inventory, id, slot, OFFHAND_SLOT)
                            .encode_packet(&mut self.out);
                        st.inventory.swap_local(slot, OFFHAND_SLOT);
                    }
                }
                let _ = r.send(Ok(()));
            }
            Command::Stack { build, reply: r } => {
                let id = self.ids.next_id();
                match build(&st.inventory, id) {
                    Ok(req) => {
                        req.encode_packet(&mut self.out);
                        self.pending_stack.push((id, self.tick, r));
                    }
                    Err(e) => {
                        let _ = r.send(Err(e));
                    }
                }
            }
            Command::OpenContainer { pos, reply: r } => {
                if self.window_wait.is_some() {
                    let _ = r.send(Err(BotError::Other(
                        "a container is already opening".into(),
                    )));
                    return;
                }
                let held = st.inventory.held().cloned().unwrap_or_default();
                let cfg = self.physics.cfg;
                let eye = st.player.eye(&cfg);
                let c = pos.center();
                let (yaw, pitch) = look_angles(eye, Vec3::new(c[0], c[1] + 0.5, c[2]));
                st.player.yaw = yaw;
                st.player.pitch = pitch;
                let rid = st.world.runtime_id(pos).unwrap_or(0);
                proto::encode_use_item(
                    &mut self.out,
                    self.protocol,
                    &UseItem {
                        action: 0,
                        block_pos: pos.to_array(),
                        face: 1,
                        hotbar_slot: st.inventory.selected_hotbar() as i32,
                        held: &held,
                        player_pos: eye.to_f32(),
                        click_pos: [0.5, 1.0, 0.5],
                        block_runtime_id: rid,
                    },
                );
                self.swing(&st);
                self.window_wait = Some(WindowWait {
                    reply: r,
                    since: self.tick,
                    opened_at: None,
                    content_received: false,
                });
            }
            Command::CloseContainer(r) => {
                if let Some(w) = st.inventory.window().map(|w| (w.id, w.kind)) {
                    proto::encode_container_close(&mut self.out, w.0, w.1);
                    st.inventory.apply_container_close(w.0);
                    self.emit(BotEvent::WindowClosed(w.0));
                }
                let _ = r.send(Ok(()));
            }
            Command::Navigate { goal, reply: r } => {
                // A collect run drives navigation itself; taking the route
                // away from it would leave it walking nowhere.
                self.abort_collect();
                if let Some(old) = self.nav.take() {
                    reply(old.reply, Err(BotError::Cancelled));
                }
                self.nav = Some(NavTask {
                    owner: NavOwner::User,
                    goal,
                    follow: None,
                    follow_anchor: None,
                    follower: PathFollower::default(),
                    needs_plan: true,
                    replans: 0,
                    reply: Some(r),
                });
            }
            Command::Follow {
                runtime_id,
                range,
                reply: r,
            } => {
                let Some(pos) = st.entities.get(runtime_id).map(|e| e.position) else {
                    let _ = r.send(Err(BotError::EntityNotFound));
                    return;
                };
                self.abort_collect();
                if let Some(old) = self.nav.take() {
                    reply(old.reply, Err(BotError::Cancelled));
                }
                self.nav = Some(NavTask {
                    owner: NavOwner::User,
                    goal: Box::new(GoalNear::new(
                        BlockPos::from_f64(pos.x, pos.y, pos.z),
                        range,
                    )),
                    follow: Some((runtime_id, range)),
                    follow_anchor: Some(pos),
                    follower: PathFollower::default(),
                    needs_plan: true,
                    replans: 0,
                    reply: Some(r),
                });
            }
            Command::StopNavigation => {
                // `stop()` stops *all* driver-owned movement, including a
                // drop-collection walk.
                self.abort_collect();
                if let Some(old) = self.nav.take() {
                    let ok = old.follow.is_some();
                    reply(
                        old.reply,
                        if ok { Ok(()) } else { Err(BotError::Cancelled) },
                    );
                }
                self.manual = Controls::default();
            }
            Command::Attack(target, r) => {
                let cfg = self.physics.cfg;
                let eye = st.player.eye(&cfg);
                let Some(e) = st.entities.get(target) else {
                    let _ = r.send(Err(BotError::EntityNotFound));
                    return;
                };
                let bb = e.aabb();
                let center = Vec3::new(
                    (bb.min.x + bb.max.x) / 2.0,
                    (bb.min.y + bb.max.y) / 2.0,
                    (bb.min.z + bb.max.z) / 2.0,
                );
                let dist = eye.distance(center);
                if dist > 3.5 + (bb.max.x - bb.min.x) {
                    let _ = r.send(Err(BotError::OutOfReach(dist)));
                    return;
                }
                let (yaw, pitch) = look_angles(eye, center);
                st.player.yaw = yaw;
                st.player.pitch = pitch;
                let held = st.inventory.held().cloned().unwrap_or_default();
                self.swing(&st);
                proto::encode_use_item_on_entity(
                    &mut self.out,
                    self.protocol,
                    target,
                    1,
                    st.inventory.selected_hotbar() as i32,
                    &held,
                    eye.to_f32(),
                );
                let _ = r.send(Ok(()));
            }
            Command::Respawn(r) => {
                Self::encode_respawn(&mut self.out, self.protocol, &st);
                let _ = r.send(Ok(()));
            }
            Command::FormResponse {
                form_id,
                response,
                reply: r,
            } => {
                let Some(i) = st.open_forms.iter().position(|f| f.0 == form_id) else {
                    let _ = r.send(Err(BotError::FormNotFound(form_id)));
                    return;
                };
                // `remove`, not `swap_remove`: the queue is ordered oldest
                // first and eviction depends on that order.
                st.open_forms.remove(i);
                match response {
                    Some(json) => proto::encode_form_response(&mut self.out, form_id, &json),
                    None => proto::encode_form_cancel(
                        &mut self.out,
                        form_id,
                        proto::FormCancelReason::UserClosed,
                    ),
                }
                let _ = r.send(Ok(()));
            }
            Command::Eat {
                network_id,
                reply: r,
            } => {
                if self.eat.is_some() {
                    let _ = r.send(Err(BotError::Other("already eating".into())));
                    return;
                }
                self.start_eat(&mut st, network_id, Some(r));
            }
            Command::CollectDrops {
                max_distance,
                reply: r,
            } => {
                self.abort_collect();
                // Collection drives navigation, so it replaces an active
                // route the same way a new route would.
                if let Some(old) = self.nav.take() {
                    reply(old.reply, Err(BotError::Cancelled));
                }
                let center = st.player.pos;
                let targets: Vec<u64> = st
                    .entities
                    .drops_near(center, max_distance as f64)
                    .into_iter()
                    .map(|d| d.0)
                    .collect();
                if targets.is_empty() {
                    let _ = r.send(Ok(0));
                    return;
                }
                let deadline = self.collect_deadline(targets.len());
                self.collect = Some(CollectTask {
                    max_distance: max_distance as f64,
                    origin: None,
                    targets,
                    current: None,
                    arrived_at: None,
                    collected: 0,
                    started: self.tick,
                    deadline,
                    reply: Some(r),
                });
            }
            Command::Disconnect => {}
        }
    }

    fn encode_respawn(out: &mut Vec<u8>, protocol: i32, st: &BotState) {
        proto::encode_respawn_ready(out, st.runtime_id);
        proto::encode_player_action(
            out,
            protocol,
            st.runtime_id,
            block_action::RESPAWN,
            [0, 0, 0],
            -1,
        );
    }

    fn abort_dig(&mut self, d: &DigTask) {
        if d.started && d.finished_at.is_none() {
            self.actions.push(BlockAction {
                action: block_action::ABORT_BREAK,
                pos: d.pos.to_array(),
                face: d.face,
            });
        }
    }

    fn encode_command(&mut self, command: &str) {
        let uuid = uuid::Uuid::new_v4();
        proto::encode_command(
            &mut self.out,
            self.protocol,
            command,
            *uuid.as_bytes(),
            &uuid.to_string(),
        );
    }

    // ------------------------------------------------------------------
    // Inbound packets
    // ------------------------------------------------------------------

    async fn on_batch(&mut self, batch: &[u8]) -> Option<String> {
        let mut deferred: Vec<Deferred> = Vec::new();
        {
            let shared = self.shared.clone();
            let mut st = lock(&shared.state);
            for pkt in iter_packets(batch) {
                let Ok(pkt) = pkt else { break };
                match Inbound::decode(pkt.id, pkt.payload, self.protocol) {
                    Ok(inbound) => self.on_packet(&mut st, inbound, &mut deferred),
                    Err(_) => continue, // unknown layout: skip this packet only
                }
            }
        }
        let mut stop = None;
        for d in deferred {
            let res = match d {
                Deferred::Typed(p) => self.transport.send_typed(p).await,
                Deferred::Latency(ts) => self.transport.respond_latency(ts).await,
                Deferred::Stop(reason) => {
                    stop = Some(reason);
                    Ok(())
                }
            };
            if let Err(e) = res {
                return Some(e.to_string());
            }
        }
        if stop.is_none() {
            if let Err(e) = self.flush().await {
                return Some(e.to_string());
            }
        }
        stop
    }

    fn on_packet(&mut self, st: &mut BotState, p: Inbound<'_>, deferred: &mut Vec<Deferred>) {
        let cfg = self.physics.cfg;
        let eye_off = Vec3::new(0.0, cfg.eye_height, 0.0);
        match p {
            Inbound::ResourcePacksInfo => deferred.push(Deferred::Typed(vec![
                Packet::ResourcePackClientResponse(ResourcePackClientResponsePacket {
                    response_status: ResourcePackResponse::AllPacksDownloaded.as_u8(),
                    resource_pack_ids: vec![],
                }),
                Packet::ClientCacheStatus(ClientCacheStatusPacket {
                    support_client_cache: false,
                }),
            ])),
            Inbound::ResourcePackStack => {
                deferred.push(Deferred::Typed(vec![Packet::ResourcePackClientResponse(
                    ResourcePackClientResponsePacket {
                        response_status: ResourcePackResponse::Completed.as_u8(),
                        resource_pack_ids: vec![],
                    },
                )]))
            }
            Inbound::StartGame(info) => {
                // The runtime-id mode decides how every block id is read, so
                // it is taken from StartGame when the tail parsed and
                // otherwise assumed hashed (the modern default) and verified
                // against the first chunks.
                let mode = match info.policy.as_ref() {
                    Some(policy) => {
                        st.server_authoritative_breaking =
                            policy.server_authoritative_block_breaking;
                        if policy.block_network_ids_are_hashes {
                            RuntimeIdMode::Hashed
                        } else {
                            RuntimeIdMode::Sequential
                        }
                    }
                    None => RuntimeIdMode::Hashed,
                };
                self.start_game_parsed = info.policy.is_some();
                // Before 1.21.60 the item table is part of StartGame.
                if let Some(items) = info
                    .policy
                    .as_ref()
                    .filter(|p| !p.items.is_empty())
                    .map(|p| p.items.iter().map(|(n, id)| (n.to_string(), *id)))
                {
                    st.items = Arc::new(ItemRegistry::from_entries(items));
                }
                self.set_registry(st, mode);
                st.world.set_dimension(info.dimension);
                st.runtime_id = info.runtime_id;
                st.unique_id = info.unique_id;
                st.game_mode = info.game_mode;
                let feet = Vec3::from_f32(info.position) - eye_off;
                st.player = PlayerState::new(feet);
                st.player.yaw = info.yaw;
                st.player.pitch = info.pitch;
                st.world.set_center(st.player.block_pos());
                deferred.push(Deferred::Typed(vec![Packet::RequestChunkRadius(
                    RequestChunkRadiusPacket {
                        radius: self.shared.config.chunk_radius,
                        max_radius: self.shared.config.chunk_radius.clamp(1, 255) as u8,
                    },
                )]));
            }
            Inbound::PlayStatus(status) => {
                if status == play_status::PLAYER_SPAWN && !st.spawned {
                    st.spawned = true;
                    deferred.push(Deferred::Typed(vec![Packet::SetLocalPlayerAsInitialized(
                        SetLocalPlayerAsInitializedPacket {
                            runtime_entity_id: st.runtime_id,
                        },
                    )]));
                    self.emit(BotEvent::Spawned);
                } else if status != play_status::LOGIN_SUCCESS
                    && status != play_status::PLAYER_SPAWN
                {
                    deferred.push(Deferred::Stop(format!("play status {status}")));
                }
            }
            Inbound::Disconnect(reason) => {
                self.emit(BotEvent::Kicked(reason.clone()));
                deferred.push(Deferred::Stop(format!("kicked: {reason}")));
            }
            Inbound::Text(msg) => {
                if msg.message.is_empty()
                    || (msg.kind == text_type::CHAT && msg.source == st.username)
                {
                    return;
                }
                self.emit(BotEvent::Chat {
                    sender: msg.source,
                    message: msg.message,
                    kind: msg.kind,
                });
            }
            Inbound::SetTime(t) => st.time_of_day = t,
            Inbound::NetworkStackLatency {
                timestamp,
                needs_response,
            } => {
                if needs_response {
                    deferred.push(Deferred::Latency(timestamp));
                }
            }
            Inbound::LevelChunk(payload) => {
                if let Ok(chunk) = LevelChunk::decode(payload) {
                    let _ = st.world.insert_level_chunk(&chunk);
                    self.verify_runtime_id_mode(st);
                }
            }
            Inbound::SubChunk(payload) => {
                let mut entries = Vec::new();
                let _ = decode_sub_chunk_packet(payload, self.protocol, |e| entries.push(e));
                let air = st.world.registry().air_runtime_id();
                for e in entries {
                    let cx = e.x;
                    let cz = e.z;
                    match e.result {
                        SubChunkResult::Success => {
                            if let Ok((_, sub)) =
                                SubChunk::decode(&mut WireReader::new(e.payload), air)
                            {
                                st.world.insert_sub_chunk(cx, e.y, cz, sub);
                            }
                        }
                        SubChunkResult::SuccessAllAir => st.world.insert_air_sub_chunk(cx, e.y, cz),
                        _ => st.world.mark_sub_chunk_unavailable(cx, e.y, cz),
                    }
                }
                self.verify_runtime_id_mode(st);
            }
            Inbound::UpdateBlock {
                pos,
                runtime_id,
                layer,
            } => {
                let pos = BlockPos::new(pos[0], pos[1], pos[2]);
                st.world.set_block(pos, runtime_id, layer as usize);
                if layer == 0 {
                    self.on_block_changed(st, pos, runtime_id);
                    self.emit(BotEvent::BlockUpdate { pos, runtime_id });
                }
            }
            Inbound::ItemRegistry(p) => {
                if let Ok(reg) = ItemRegistry::decode_shared(p) {
                    st.items = reg;
                }
            }
            Inbound::CraftingData(p) => {
                if let Ok(book) = RecipeBook::decode_shared(p) {
                    st.recipes = book;
                }
            }
            Inbound::InventoryContent(p) => {
                if let Ok((window, items)) = decode_inventory_content_for(p, self.protocol) {
                    st.inventory.apply_content(window, items);
                    if let Some(w) = self.window_wait.as_mut() {
                        if st
                            .inventory
                            .window()
                            .is_some_and(|win| win.id as u32 == window)
                        {
                            w.content_received = true;
                        }
                    }
                }
            }
            Inbound::InventorySlot(p) => {
                if let Ok((window, slot, item)) = decode_inventory_slot_for(p, self.protocol) {
                    st.inventory.apply_slot(window, slot, item);
                }
            }
            Inbound::ContainerOpen(p) => {
                if let Ok(open) = ContainerOpen::decode_for(p, self.protocol) {
                    let id = open.window_id;
                    st.inventory.apply_container_open(open);
                    if let Some(w) = self.window_wait.as_mut() {
                        w.opened_at = Some(self.tick);
                    }
                    self.emit(BotEvent::WindowOpened(id));
                }
            }
            Inbound::ContainerClose(p) => {
                if let Ok((window, _, _)) = decode_container_close(p) {
                    st.inventory.apply_container_close(window);
                    self.emit(BotEvent::WindowClosed(window));
                }
            }
            Inbound::ItemStackResponse(p) => {
                if let Ok(responses) = decode_item_stack_response(p) {
                    for resp in responses {
                        if resp.status == 0 {
                            st.inventory.apply_stack_response(&resp);
                        }
                        if let Some(i) = self
                            .pending_stack
                            .iter()
                            .position(|x| x.0 == resp.request_id)
                        {
                            let (_, _, r) = self.pending_stack.swap_remove(i);
                            let _ = r.send(if resp.status == 0 {
                                Ok(resp)
                            } else {
                                Err(BotError::Rejected(format!(
                                    "item stack request status {}",
                                    resp.status
                                )))
                            });
                        }
                    }
                }
            }
            Inbound::Spawn(s) => {
                if s.runtime_id == st.runtime_id {
                    return;
                }
                let center = st.player.pos;
                let item = s.item.as_ref().map(|i| (i.network_id, i.count));
                let pos = Vec3::from_f32(s.position);
                let feet = if s.username.is_some() {
                    pos - eye_off
                } else {
                    pos
                };
                let stored = st.entities.spawn(
                    center,
                    s.runtime_id,
                    s.unique_id.unwrap_or(s.runtime_id as i64),
                    s.kind,
                    s.username,
                    feet,
                    Vec3::from_f32(s.velocity),
                    s.yaw,
                    s.pitch,
                    if s.kind == "minecraft:item" {
                        item
                    } else {
                        None
                    },
                );
                if stored {
                    if let (Some(e), Some(held)) =
                        (st.entities.get_mut(s.runtime_id), s.item.as_ref())
                    {
                        if e.is_player() {
                            e.held_item = held.network_id;
                        }
                    }
                    self.emit(BotEvent::EntitySpawned(s.runtime_id));
                }
            }
            Inbound::RemoveActor(unique) => {
                if let Some(e) = st.entities.remove_unique(unique) {
                    self.emit(BotEvent::EntityRemoved(e.runtime_id));
                }
            }
            Inbound::TakeItemActor { item, taker } => {
                // Only a pickup by *this* bot counts towards a collect run;
                // another player or a mob taking the item is just a removal.
                if taker == st.runtime_id {
                    if let Some(c) = self.collect.as_mut() {
                        if c.current.is_some_and(|(id, _)| id == item) || c.targets.contains(&item)
                        {
                            c.collected = c.collected.saturating_add(1);
                        }
                    }
                }
                if st.entities.remove_runtime(item).is_some() {
                    self.emit(BotEvent::EntityRemoved(item));
                }
            }
            Inbound::MoveActor {
                runtime_id,
                position,
                yaw,
                pitch,
                on_ground,
                ..
            } => {
                if let Some(e) = st.entities.get_mut(runtime_id) {
                    let player = e.is_player();
                    if let Some(p) = position {
                        let off = if player { cfg.eye_height } else { 0.0 };
                        if let Some(x) = p[0] {
                            e.position.x = x as f64;
                        }
                        if let Some(y) = p[1] {
                            e.position.y = y as f64 - off;
                        }
                        if let Some(z) = p[2] {
                            e.position.z = z as f64;
                        }
                    }
                    if let Some(y) = yaw {
                        e.yaw = y;
                    }
                    if let Some(p) = pitch {
                        e.pitch = p;
                    }
                    e.on_ground = on_ground;
                }
            }
            Inbound::MovePlayer(m) => {
                let feet = Vec3::from_f32(m.position) - eye_off;
                if m.runtime_id == st.runtime_id && m.mode == proto::move_mode::ROTATION {
                    // Rotation-only update: keep position and velocity.
                    st.player.yaw = m.yaw;
                    st.player.pitch = m.pitch;
                } else if m.runtime_id == st.runtime_id {
                    // Teleport / reset from the server: snap, drop velocity
                    // and acknowledge on the next input. A teleport is
                    // unconditional, so it is applied without consulting the
                    // packet's tick field.
                    self.apply_server_position(st, feet, None, m.on_ground);
                    st.player.yaw = m.yaw;
                    st.player.pitch = m.pitch;
                    self.extra_flags |= input_flags::HANDLED_TELEPORT;
                    self.emit(BotEvent::Teleported(feet));
                } else if let Some(e) = st.entities.get_mut(m.runtime_id) {
                    e.position = feet;
                    e.yaw = m.yaw;
                    e.pitch = m.pitch;
                    e.on_ground = m.on_ground;
                }
            }
            Inbound::CorrectMove(c) => {
                if c.prediction_type != proto::prediction_type::PLAYER {
                    return; // vehicle predictions are not simulated
                }
                if c.tick > self.tick {
                    // The tick is the bot's own input tick echoed back, so a
                    // tick it has not sent yet cannot be answering it.
                    return;
                }
                let feet = Vec3::from_f32(c.position) - eye_off;
                let velocity = Vec3::from_f32(c.velocity);
                st.corrections += 1;
                self.apply_server_position(st, feet, Some(velocity), c.on_ground);
                if let Some([pitch, yaw]) = c.rotation {
                    st.player.pitch = pitch;
                    st.player.yaw = yaw;
                }
                self.emit(BotEvent::MovementCorrected {
                    position: feet,
                    tick: c.tick,
                });
            }
            Inbound::SetActorMotion {
                runtime_id,
                velocity,
            } => {
                let v = Vec3::from_f32(velocity);
                if runtime_id == st.runtime_id {
                    // Clamped: the value is simulated, and an absurd one would
                    // make the collision sweep enumerate a huge region.
                    st.player.set_velocity(v);
                } else if let Some(e) = st.entities.get_mut(runtime_id) {
                    e.velocity = v;
                }
            }
            Inbound::SetHealth(h) => self.set_health(st, h as f32, None, deferred),
            Inbound::Attributes { runtime_id, values } => {
                if runtime_id == st.runtime_id {
                    let mut health = None;
                    let mut food = None;
                    for (name, value, _max) in values {
                        match name {
                            "minecraft:health" => health = Some(value),
                            "minecraft:player.hunger" => food = Some(value),
                            "minecraft:movement" => st.player.movement_speed = value as f64,
                            _ => {}
                        }
                    }
                    if let Some(h) = health {
                        self.set_health(st, h, food, deferred);
                    } else if let Some(f) = food {
                        st.food = f;
                        self.emit(BotEvent::Health {
                            health: st.health,
                            food: f,
                        });
                    }
                }
            }
            Inbound::Respawn {
                position, state, ..
            } => {
                if state == 1 {
                    let feet = Vec3::from_f32(position) - eye_off;
                    self.apply_server_position(st, feet, None, false);
                    if st.dead {
                        st.dead = false;
                        st.health = 20.0;
                        self.emit(BotEvent::Respawned);
                    }
                }
            }
            Inbound::ChangeDimension {
                dimension,
                position,
            } => {
                st.world.set_dimension(dimension);
                let feet = Vec3::from_f32(position) - eye_off;
                self.apply_server_position(st, feet, None, false);
                // Every entity from the old dimension is gone; a collect run
                // chasing one of them can never finish.
                self.abort_collect();
                st.entities = crate::entity::EntityTable::new(
                    self.shared.config.entity_capacity,
                    self.shared.config.entity_radius,
                );
                proto::encode_player_action(
                    &mut self.out,
                    self.protocol,
                    st.runtime_id,
                    block_action::DIMENSION_CHANGE_DONE,
                    [0, 0, 0],
                    0,
                );
                self.emit(BotEvent::Teleported(feet));
            }
            Inbound::MobEquipment {
                runtime_id,
                item,
                hotbar_slot,
                window,
            } => {
                if runtime_id == st.runtime_id {
                    if window == 0 && hotbar_slot < 9 {
                        st.inventory.set_selected_hotbar(hotbar_slot);
                    }
                } else if let Some(e) = st.entities.get_mut(runtime_id) {
                    e.held_item = item.network_id;
                }
            }
            Inbound::PlayerHotbar {
                slot,
                window,
                select,
            } => {
                if select && window == 0 && slot < 9 {
                    st.inventory.set_selected_hotbar(slot as u8);
                }
            }
            Inbound::ModalForm { form_id, data } => {
                let data = if data.len() > MAX_FORM_JSON {
                    &data[..floor_char_boundary(data, MAX_FORM_JSON)]
                } else {
                    data
                };
                // Re-sending a form id replaces the stored copy, so drop it
                // before measuring the queue: a replacement must not evict an
                // unrelated form.
                st.open_forms.retain(|f| f.0 != form_id);
                // Keep a bounded number of unanswered forms per bot. The queue
                // is kept in arrival order, so index 0 is the oldest.
                while st.open_forms.len() >= MAX_OPEN_FORMS {
                    let (old, _) = st.open_forms.remove(0);
                    proto::encode_form_cancel(
                        &mut self.out,
                        old,
                        proto::FormCancelReason::UserBusy,
                    );
                }
                st.open_forms.push((form_id, data.to_string()));
                self.emit(BotEvent::FormRequest {
                    form_id,
                    data: data.to_string(),
                });
            }
            Inbound::CloseForm => {
                st.open_forms.clear();
                self.emit(BotEvent::FormsClosed);
            }
            Inbound::ActorEvent {
                runtime_id,
                event,
                data,
            } => {
                // `Feed` is broadcast while eating. After the consume was
                // sent, the server's eating event for our food confirms the
                // bite even if the hunger bar was already full enough that
                // it does not visibly rise (e.g. saturation-only foods).
                if runtime_id == st.runtime_id && event == proto::actor_event::FEED {
                    if let Some(e) = self.eat.take() {
                        if e.consumed_at.is_some() && data >> 16 == e.item {
                            let item = e.item;
                            self.finish_eat(st, e, Ok(item));
                        } else {
                            self.eat = Some(e);
                        }
                    }
                }
            }
            Inbound::ChunkRadiusUpdated(_) | Inbound::Other(_) => {}
        }
    }

    fn set_health(
        &mut self,
        st: &mut BotState,
        health: f32,
        food: Option<f32>,
        _deferred: &mut [Deferred],
    ) {
        st.health = health;
        if let Some(f) = food {
            st.food = f;
        }
        self.emit(BotEvent::Health {
            health,
            food: st.food,
        });
        if health <= 0.0 && !st.dead {
            st.dead = true;
            // Nothing driver-owned survives a death: the server resets the
            // player, so every in-flight task must be answered now instead
            // of waiting for a timeout that can never be satisfied.
            self.abort_collect();
            if let Some(n) = self.nav.take() {
                reply(n.reply, Err(BotError::Cancelled));
            }
            if let Some(d) = self.dig.take() {
                reply(d.reply, Err(BotError::Cancelled));
            }
            if let Some(e) = self.eat.take() {
                reply(e.reply, Err(BotError::Cancelled));
            }
            if let Some(p) = self.place.take() {
                reply(p.reply, Err(BotError::Cancelled));
            }
            self.emit(BotEvent::Death);
            if self.shared.config.auto_respawn {
                Self::encode_respawn(&mut self.out, self.protocol, st);
            }
        }
    }

    /// Applies an authoritative position from the server (correction,
    /// teleport, respawn or dimension change).
    ///
    /// * snaps the simulated player and replaces its velocity (the server's
    ///   velocity for corrections, zero otherwise), clearing fall distance;
    /// * resets the delta sent with the next input so the server does not
    ///   see a jump from the stale position;
    /// * tells an active navigation's [`PathFollower`] about the jump so the
    ///   path is kept (or re-planned) instead of reported as stuck.
    ///
    /// Note that the local tick counter is deliberately left alone: the tick
    /// a correction carries is the bot's *own* input tick echoed back, and the
    /// counter doubles as the clock every pending task measures its timeout
    /// against, so it stays a monotonic local clock.
    fn apply_server_position(
        &mut self,
        st: &mut BotState,
        feet: Vec3,
        velocity: Option<Vec3>,
        on_ground: bool,
    ) {
        st.player.apply_correction(feet, velocity, on_ground);
        st.player.jump_cooldown = 0;
        st.world.set_center(st.player.block_pos());
        self.last_delta = Vec3::ZERO;
        if let Some(nav) = self.nav.as_mut() {
            if !nav
                .follower
                .on_position_corrected(feet, CORRECTION_REPLAN_DISTANCE)
            {
                nav.needs_plan = true;
            }
        }
    }

    fn on_block_changed(&mut self, st: &BotState, pos: BlockPos, runtime_id: u32) {
        let reg = st.world.registry();
        let air = reg.get(runtime_id).is_air();
        if let Some(d) = self.dig.as_ref() {
            if d.pos == pos && d.started {
                let d = self.dig.take().expect("checked");
                if air && d.collect_drops {
                    self.queue_drop_collection(d.pos, d.reply);
                } else if air {
                    reply(d.reply, Ok(()));
                } else if d.finished_at.is_some() && runtime_id == d.original {
                    reply(
                        d.reply,
                        Err(BotError::Rejected("server restored the block".into())),
                    );
                } else {
                    self.dig = Some(d);
                }
            }
        }
        if let Some(p) = self.place.as_ref() {
            if p.target == pos {
                let p = self.place.take().expect("checked");
                if air {
                    reply(
                        p.reply,
                        Err(BotError::Rejected("server removed the placed block".into())),
                    );
                } else {
                    reply(p.reply, Ok(()));
                }
            }
        }
    }
}

fn fail_command(cmd: Command) {
    fail_command_with(cmd, BotError::Disconnected);
}

fn fail_command_with(cmd: Command, e: BotError) {
    match cmd {
        Command::Chat(_, r)
        | Command::Slash(_, r)
        | Command::Dig { reply: r, .. }
        | Command::Place { reply: r, .. }
        | Command::SelectHotbar(_, r)
        | Command::Equip { reply: r, .. }
        | Command::CloseContainer(r)
        | Command::Navigate { reply: r, .. }
        | Command::Follow { reply: r, .. }
        | Command::Attack(_, r)
        | Command::Respawn(r) => {
            let _ = r.send(Err(e));
        }
        Command::Stack { reply: r, .. } => {
            let _ = r.send(Err(e));
        }
        Command::OpenContainer { reply: r, .. } => {
            let _ = r.send(Err(e));
        }
        Command::FormResponse { reply: r, .. } => {
            let _ = r.send(Err(e));
        }
        Command::Eat { reply: r, .. } => {
            let _ = r.send(Err(e));
        }
        Command::CollectDrops { reply: r, .. } => {
            let _ = r.send(Err(e));
        }
        Command::Look { .. }
        | Command::SetControls(_)
        | Command::StopDig
        | Command::StopNavigation
        | Command::Disconnect => {}
    }
}
