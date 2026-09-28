//! The per-bot task: owns the transport, runs the 20 Hz tick loop, applies
//! inbound packets to [`BotState`] and drives action state machines.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use torchflower_engine::bedrock::protocol_adapter::observe_start_game;
use torchflower_inventory::{
    decode_container_close, decode_inventory_content, decode_inventory_slot,
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
use torchflower_world::chunk::{decode_sub_chunk_packet, encode_sub_chunk_request};
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

struct NavTask {
    goal: Box<dyn Goal + Send>,
    follow: Option<(u64, f32)>,
    follow_anchor: Option<Vec3>,
    follower: PathFollower,
    needs_plan: bool,
    replans: u32,
    reply: Option<Reply<()>>,
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
            protocol,
            last_delta: Vec3::ZERO,
        }
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
        let digging = self.update_dig(st);
        self.update_nav(st, &mut controls);
        if digging && self.nav.is_none() {
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
                encode_sub_chunk_request(&mut self.out, st.world.dimension(), base, &reqs);
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
        proto::encode_swing(&mut self.out, st.runtime_id);
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
                        st.runtime_id,
                        block_action::STOP_BREAK,
                        d.pos.to_array(),
                        d.face,
                    );
                    let held = st.inventory.held().cloned().unwrap_or_default();
                    let rid = st.world.runtime_id(d.pos).unwrap_or(0);
                    proto::encode_use_item(
                        &mut self.out,
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
            reply(
                d.reply,
                if broken {
                    Ok(())
                } else {
                    Err(BotError::Rejected("block break not confirmed".into()))
                },
            );
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
        proto::encode_mob_equipment(&mut self.out, st.runtime_id, &item, target);
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
            Command::Dig { pos, reply: r } => {
                if let Some(old) = self.dig.take() {
                    self.abort_dig(&old);
                    reply(old.reply, Err(BotError::Cancelled));
                }
                self.dig = Some(DigTask {
                    pos,
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
                if let Some(old) = self.nav.take() {
                    reply(old.reply, Err(BotError::Cancelled));
                }
                self.nav = Some(NavTask {
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
                if let Some(old) = self.nav.take() {
                    reply(old.reply, Err(BotError::Cancelled));
                }
                let Some(pos) = st.entities.get(runtime_id).map(|e| e.position) else {
                    let _ = r.send(Err(BotError::EntityNotFound));
                    return;
                };
                self.nav = Some(NavTask {
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
                    target,
                    1,
                    st.inventory.selected_hotbar() as i32,
                    &held,
                    eye.to_f32(),
                );
                let _ = r.send(Ok(()));
            }
            Command::Respawn(r) => {
                Self::encode_respawn(&mut self.out, &st);
                let _ = r.send(Ok(()));
            }
            Command::Disconnect => {}
        }
    }

    fn encode_respawn(out: &mut Vec<u8>, st: &BotState) {
        proto::encode_respawn_ready(out, st.runtime_id);
        proto::encode_player_action(out, st.runtime_id, block_action::RESPAWN, [0, 0, 0], -1);
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
        proto::encode_command(&mut self.out, command, *uuid.as_bytes(), &uuid.to_string());
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
            Inbound::StartGame(info, raw) => {
                let observed = observe_start_game(raw);
                let hashed = observed
                    .as_ref()
                    .and_then(|o| o.block_network_ids_are_hashes)
                    .unwrap_or(false);
                st.server_authoritative_breaking = observed
                    .as_ref()
                    .and_then(|o| o.server_authoritative_block_breaking)
                    .unwrap_or(true);
                let mode = if hashed {
                    RuntimeIdMode::Hashed
                } else {
                    RuntimeIdMode::Sequential
                };
                let registry =
                    shared_registry(self.shared.config.canonical_block_states.as_ref(), mode);
                st.world = SparseWorld::new(registry, self.shared.config.window);
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
                if let Ok((window, items)) = decode_inventory_content(p) {
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
                if let Ok((window, slot, item)) = decode_inventory_slot(p) {
                    st.inventory.apply_slot(window, slot, item);
                }
            }
            Inbound::ContainerOpen(p) => {
                if let Ok(open) = ContainerOpen::decode(p) {
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
            Inbound::TakeItemActor { item, .. } => {
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
            Inbound::MovePlayer {
                runtime_id,
                position,
                pitch,
                yaw,
                on_ground,
                ..
            } => {
                let feet = Vec3::from_f32(position) - eye_off;
                if runtime_id == st.runtime_id {
                    st.player.apply_correction(feet, None, on_ground);
                    st.player.yaw = yaw;
                    st.player.pitch = pitch;
                    st.world.set_center(st.player.block_pos());
                    self.extra_flags |= input_flags::HANDLED_TELEPORT;
                    self.emit(BotEvent::Teleported(feet));
                } else if let Some(e) = st.entities.get_mut(runtime_id) {
                    e.position = feet;
                    e.yaw = yaw;
                    e.pitch = pitch;
                    e.on_ground = on_ground;
                }
            }
            Inbound::CorrectMove {
                position,
                delta,
                on_ground,
            } => {
                let feet = Vec3::from_f32(position) - eye_off;
                st.player
                    .apply_correction(feet, Some(Vec3::from_f32(delta)), on_ground);
            }
            Inbound::SetActorMotion {
                runtime_id,
                velocity,
            } => {
                let v = Vec3::from_f32(velocity);
                if runtime_id == st.runtime_id {
                    st.player.vel = v;
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
                    st.player.apply_correction(feet, None, false);
                    st.world.set_center(st.player.block_pos());
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
                st.player.apply_correction(feet, None, false);
                st.world.set_center(st.player.block_pos());
                st.entities = crate::entity::EntityTable::new(
                    self.shared.config.entity_capacity,
                    self.shared.config.entity_radius,
                );
                proto::encode_player_action(
                    &mut self.out,
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
            if let Some(n) = self.nav.take() {
                reply(n.reply, Err(BotError::Cancelled));
            }
            if let Some(d) = self.dig.take() {
                reply(d.reply, Err(BotError::Cancelled));
            }
            self.emit(BotEvent::Death);
            if self.shared.config.auto_respawn {
                Self::encode_respawn(&mut self.out, st);
            }
        }
    }

    fn on_block_changed(&mut self, st: &BotState, pos: BlockPos, runtime_id: u32) {
        let reg = st.world.registry();
        let air = reg.get(runtime_id).is_air();
        if let Some(d) = self.dig.as_ref() {
            if d.pos == pos && d.started {
                let d = self.dig.take().expect("checked");
                if air {
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
        Command::Look { .. }
        | Command::SetControls(_)
        | Command::StopDig
        | Command::StopNavigation
        | Command::Disconnect => {}
    }
}
