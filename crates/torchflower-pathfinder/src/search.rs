//! Bounded A* over feet positions in the sparse voxel world.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use torchflower_world::{BlockFlags, BlockPos, Shape, SparseWorld};

use crate::goal::Goal;

/// Cost of walking one block (ticks at walking speed).
pub const WALK_COST: f32 = 4.633;
/// Cost of sprinting one block.
pub const SPRINT_COST: f32 = 3.564;
/// Extra cost of a jump.
pub const JUMP_COST: f32 = 2.0;
/// Extra cost of placing a block.
pub const PLACE_COST: f32 = 8.0;
/// Fixed penalty for breaking a block (on top of dig ticks).
pub const DIG_PENALTY: f32 = 4.0;

/// World queries used by the planner.
pub trait PathWorld {
    /// Collision shape; `None` if unloaded.
    fn shape(&self, pos: BlockPos) -> Option<Shape>;
    /// Material flags (NONE if unknown).
    fn flags(&self, pos: BlockPos) -> BlockFlags;
    /// Ticks to break the block, or `None` if it may not be broken.
    fn dig_ticks(&self, pos: BlockPos) -> Option<u32>;
}

/// [`PathWorld`] adapter for a [`SparseWorld`] with a caller-provided dig
/// cost function (usually "best tool in the inventory").
pub struct WorldView<'a, F> {
    pub world: &'a SparseWorld,
    pub dig: F,
}

impl<'a, F> PathWorld for WorldView<'a, F>
where
    F: Fn(BlockPos) -> Option<u32>,
{
    fn shape(&self, pos: BlockPos) -> Option<Shape> {
        self.world.shape_at(pos)
    }
    fn flags(&self, pos: BlockPos) -> BlockFlags {
        self.world.flags_at(pos).unwrap_or(BlockFlags::NONE)
    }
    fn dig_ticks(&self, pos: BlockPos) -> Option<u32> {
        (self.dig)(pos)
    }
}

/// Movement primitive used to reach a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Walk,
    Diagonal,
    Ascend,
    Descend,
    Parkour,
    Swim,
    Pillar,
    Bridge,
    DigDown,
}

/// One waypoint of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// Feet position to reach.
    pub pos: BlockPos,
    pub kind: MoveKind,
    /// Blocks that must be broken before moving (in order).
    pub dig: [Option<BlockPos>; 3],
    /// Block position that must be filled (placed) before moving.
    pub place: Option<BlockPos>,
}

/// Planner options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathOptions {
    pub max_nodes: usize,
    pub timeout: Duration,
    pub allow_dig: bool,
    /// Number of scaffolding blocks available for bridging / pillaring.
    pub scaffold_blocks: u32,
    pub allow_parkour: bool,
    pub allow_sprint: bool,
    pub max_drop: i32,
}

impl Default for PathOptions {
    fn default() -> Self {
        Self {
            max_nodes: 4_000,
            timeout: Duration::from_millis(40),
            allow_dig: true,
            scaffold_blocks: 0,
            allow_parkour: true,
            allow_sprint: true,
            max_drop: 3,
        }
    }
}

/// Search outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStatus {
    /// The goal is reached by the path.
    Complete,
    /// Node/time budget exhausted: path leads to the most promising node.
    Partial,
    /// No progress possible.
    NoPath,
}

/// Result of [`find_path`].
#[derive(Debug, Clone, PartialEq)]
pub struct PathResult {
    pub status: PathStatus,
    pub steps: Vec<Step>,
    pub cost: f32,
    pub visited: usize,
}

#[derive(Clone, Copy)]
struct Node {
    pos: BlockPos,
    g: f32,
    parent: u32,
    step: Step,
    scaffold_used: u16,
}

#[derive(PartialEq)]
struct Open {
    f: f32,
    idx: u32,
}
impl Eq for Open {}
impl PartialOrd for Open {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Open {
    fn cmp(&self, o: &Self) -> Ordering {
        o.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal)
    }
}

struct Ctx<'a, W: PathWorld> {
    w: &'a W,
    opt: &'a PathOptions,
}

impl<W: PathWorld> Ctx<'_, W> {
    fn passable(&self, p: BlockPos) -> bool {
        match self.w.shape(p) {
            Some(Shape::Empty) => {
                let f = self.w.flags(p);
                !f.contains(BlockFlags::LAVA) && !f.contains(BlockFlags::DANGEROUS)
            }
            _ => false,
        }
    }

    fn is_water(&self, p: BlockPos) -> bool {
        self.w.flags(p).contains(BlockFlags::WATER)
    }

    fn solid_floor(&self, p: BlockPos) -> bool {
        match self.w.shape(p) {
            Some(s) if !s.is_empty() => {
                let top = s.top();
                top > 0.4 && top <= 1.0 && !self.w.flags(p).contains(BlockFlags::DANGEROUS)
            }
            _ => false,
        }
    }

    fn standable(&self, p: BlockPos) -> bool {
        self.passable(p)
            && self.passable(p.offset(0, 1, 0))
            && (self.solid_floor(p.offset(0, -1, 0))
                || self.is_water(p)
                || self.w.flags(p).contains(BlockFlags::CLIMBABLE))
    }

    /// Cost to make `p` passable by digging, or `None`.
    fn clear_cost(&self, p: BlockPos) -> Option<f32> {
        if self.passable(p) {
            return Some(0.0);
        }
        if !self.opt.allow_dig {
            return None;
        }
        let shape = self.w.shape(p)?;
        if shape.is_empty() {
            return None; // dangerous non-solid (lava, fire)
        }
        // Never dig blocks that would let liquid or falling blocks in.
        let above = p.offset(0, 1, 0);
        let fa = self.w.flags(above);
        if fa.contains(BlockFlags::LIQUID) || fa.contains(BlockFlags::GRAVITY) {
            return None;
        }
        let ticks = self.w.dig_ticks(p)?;
        Some(ticks as f32 + DIG_PENALTY)
    }
}

fn step(pos: BlockPos, kind: MoveKind) -> Step {
    Step {
        pos,
        kind,
        dig: [None; 3],
        place: None,
    }
}

const CARDINALS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
const DIAGONALS: [(i32, i32); 4] = [(1, 1), (1, -1), (-1, 1), (-1, -1)];

fn neighbors<W: PathWorld>(c: &Ctx<'_, W>, n: &Node, out: &mut Vec<(Step, f32, u16)>) {
    let p = n.pos;
    let walk = if c.opt.allow_sprint {
        SPRINT_COST
    } else {
        WALK_COST
    };
    let in_water = c.is_water(p);
    let scaffold_left = c.opt.scaffold_blocks.saturating_sub(n.scaffold_used as u32);

    for (dx, dz) in CARDINALS {
        let t = p.offset(dx, 0, dz);
        let head = t.offset(0, 1, 0);
        // Walk / dig-through.
        if c.solid_floor(t.offset(0, -1, 0)) || c.is_water(t) {
            if let (Some(a), Some(b)) = (c.clear_cost(t), c.clear_cost(head)) {
                let mut s = step(
                    t,
                    if c.is_water(t) {
                        MoveKind::Swim
                    } else {
                        MoveKind::Walk
                    },
                );
                let mut i = 0;
                for (cost, pos) in [(b, head), (a, t)] {
                    if cost > 0.0 {
                        s.dig[i] = Some(pos);
                        i += 1;
                    }
                }
                let base = if in_water || c.is_water(t) {
                    WALK_COST * 2.0
                } else {
                    walk
                };
                out.push((s, base + a + b, 0));
            }
        } else if scaffold_left > 0 && c.passable(t) && c.passable(head) {
            // Bridge: place a block under the target.
            let below = t.offset(0, -1, 0);
            if c.w.shape(below) == Some(Shape::Empty)
                && !c.w.flags(below).contains(BlockFlags::LAVA)
            {
                let mut s = step(t, MoveKind::Bridge);
                s.place = Some(below);
                out.push((s, WALK_COST * 2.0 + PLACE_COST, 1));
            }
        }

        // Ascend one block.
        let up = t.offset(0, 1, 0);
        if c.solid_floor(t) {
            if let (Some(h0), Some(a), Some(b)) = (
                c.clear_cost(p.offset(0, 2, 0)),
                c.clear_cost(up),
                c.clear_cost(up.offset(0, 1, 0)),
            ) {
                let mut s = step(up, MoveKind::Ascend);
                let mut i = 0;
                for (cost, pos) in [(h0, p.offset(0, 2, 0)), (b, up.offset(0, 1, 0)), (a, up)] {
                    if cost > 0.0 && i < 3 {
                        s.dig[i] = Some(pos);
                        i += 1;
                    }
                }
                out.push((s, walk + JUMP_COST + h0 + a + b, 0));
            }
        }

        // Descend / drop.
        if c.passable(t) && c.passable(head) && !c.solid_floor(t.offset(0, -1, 0)) && !c.is_water(t)
        {
            let max_drop = c.opt.max_drop.max(1);
            let mut y = 1;
            while y <= max_drop + 16 {
                let land = t.offset(0, -y, 0);
                if c.is_water(land) {
                    out.push((step(land, MoveKind::Descend), walk + y as f32 * 0.8, 0));
                    break;
                }
                if !c.passable(land) {
                    break;
                }
                if c.solid_floor(land.offset(0, -1, 0)) {
                    if y <= max_drop {
                        out.push((
                            step(land, MoveKind::Descend),
                            walk + y as f32 * 0.8 + if y > 1 { 1.0 } else { 0.0 },
                            0,
                        ));
                    }
                    break;
                }
                y += 1;
            }

            // Parkour over a 1–2 block gap.
            if c.opt.allow_parkour && !in_water && c.passable(p.offset(0, 2, 0)) {
                for gap in 1..=2 {
                    let mut clear = true;
                    for k in 1..=gap {
                        let g = p.offset(dx * k, 0, dz * k);
                        if !(c.passable(g)
                            && c.passable(g.offset(0, 1, 0))
                            && c.passable(g.offset(0, 2, 0)))
                        {
                            clear = false;
                            break;
                        }
                    }
                    if !clear {
                        break;
                    }
                    let land = p.offset(dx * (gap + 1), 0, dz * (gap + 1));
                    if c.standable(land) && c.passable(land.offset(0, 2, 0)) {
                        out.push((
                            step(land, MoveKind::Parkour),
                            SPRINT_COST * (gap + 1) as f32 + JUMP_COST * 2.0,
                            0,
                        ));
                        break;
                    }
                }
            }
        }
    }

    // Diagonals (no digging).
    for (dx, dz) in DIAGONALS {
        let t = p.offset(dx, 0, dz);
        if c.standable(t)
            && c.passable(p.offset(dx, 0, 0))
            && c.passable(p.offset(dx, 1, 0))
            && c.passable(p.offset(0, 0, dz))
            && c.passable(p.offset(0, 1, dz))
        {
            let base = if in_water { WALK_COST * 2.0 } else { walk };
            out.push((
                step(t, MoveKind::Diagonal),
                base * std::f32::consts::SQRT_2,
                0,
            ));
        }
    }

    // Swim up / climb.
    let up = p.offset(0, 1, 0);
    if (in_water || c.w.flags(p).contains(BlockFlags::CLIMBABLE))
        && c.passable(up.offset(0, 1, 0))
        && (c.is_water(up) || c.passable(up))
    {
        out.push((step(up, MoveKind::Swim), WALK_COST * 1.5, 0));
    }
    if in_water && c.passable(p.offset(0, -1, 0)) {
        out.push((step(p.offset(0, -1, 0), MoveKind::Swim), WALK_COST * 1.5, 0));
    }

    // Pillar up.
    if scaffold_left > 0 && !in_water && c.solid_floor(p.offset(0, -1, 0)) {
        if let Some(h) = c.clear_cost(p.offset(0, 2, 0)) {
            let mut s = step(up, MoveKind::Pillar);
            s.place = Some(p);
            if h > 0.0 {
                s.dig[0] = Some(p.offset(0, 2, 0));
            }
            out.push((s, JUMP_COST + PLACE_COST + h + WALK_COST, 1));
        }
    }

    // Dig down.
    let below = p.offset(0, -1, 0);
    if c.opt.allow_dig && !in_water && c.solid_floor(below.offset(0, -1, 0)) {
        if let Some(cost) = c.clear_cost(below) {
            if cost > 0.0 {
                let mut s = step(below, MoveKind::DigDown);
                s.dig[0] = Some(below);
                out.push((s, cost + 2.0, 0));
            }
        }
    }
}

/// Runs A* from `start` (feet block) to `goal`.
pub fn find_path<W: PathWorld, G: Goal>(
    world: &W,
    start: BlockPos,
    goal: &G,
    opt: &PathOptions,
) -> PathResult {
    let began = Instant::now();
    let ctx = Ctx { w: world, opt };
    let mut nodes: Vec<Node> = Vec::with_capacity(opt.max_nodes.min(4096));
    let mut index: HashMap<BlockPos, u32> = HashMap::with_capacity(opt.max_nodes.min(4096));
    let mut open = BinaryHeap::new();
    let mut scratch = Vec::with_capacity(24);

    nodes.push(Node {
        pos: start,
        g: 0.0,
        parent: u32::MAX,
        step: step(start, MoveKind::Walk),
        scaffold_used: 0,
    });
    index.insert(start, 0);
    open.push(Open {
        f: goal.heuristic(start),
        idx: 0,
    });
    let mut best = (goal.heuristic(start), 0u32);
    let mut found: Option<u32> = None;
    let mut closed = vec![false; 1];

    while let Some(Open { idx, .. }) = open.pop() {
        if closed[idx as usize] {
            continue;
        }
        closed[idx as usize] = true;
        let node = nodes[idx as usize];
        if goal.is_end(node.pos) {
            found = Some(idx);
            break;
        }
        let h = goal.heuristic(node.pos);
        if h < best.0 {
            best = (h, idx);
        }
        if nodes.len() >= opt.max_nodes || began.elapsed() > opt.timeout {
            break;
        }
        scratch.clear();
        neighbors(&ctx, &node, &mut scratch);
        for (s, cost, used) in scratch.drain(..) {
            let g = node.g + cost;
            let scaffold_used = node.scaffold_used + used;
            match index.get(&s.pos) {
                Some(&i) if nodes[i as usize].g <= g || closed[i as usize] => continue,
                Some(&i) => {
                    let n = &mut nodes[i as usize];
                    n.g = g;
                    n.parent = idx;
                    n.step = s;
                    n.scaffold_used = scaffold_used;
                    open.push(Open {
                        f: g + goal.heuristic(s.pos),
                        idx: i,
                    });
                }
                None => {
                    let i = nodes.len() as u32;
                    nodes.push(Node {
                        pos: s.pos,
                        g,
                        parent: idx,
                        step: s,
                        scaffold_used,
                    });
                    closed.push(false);
                    index.insert(s.pos, i);
                    open.push(Open {
                        f: g + goal.heuristic(s.pos),
                        idx: i,
                    });
                }
            }
        }
    }

    let (status, end) = match found {
        Some(i) => (PathStatus::Complete, i),
        None if best.1 != 0 => (PathStatus::Partial, best.1),
        None => (PathStatus::NoPath, 0),
    };
    let mut steps = Vec::new();
    let mut cur = end;
    while cur != 0 && cur != u32::MAX {
        let n = &nodes[cur as usize];
        steps.push(n.step);
        cur = n.parent;
    }
    steps.reverse();
    PathResult {
        status,
        cost: nodes[end as usize].g,
        steps,
        visited: nodes.len(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::goal::{GoalBlock, GoalNear};
    use std::collections::HashMap;

    /// Flat floor at y = 63; custom overrides.
    #[derive(Default)]
    pub struct Grid {
        pub blocks: HashMap<BlockPos, (Shape, BlockFlags)>,
        pub floor: i32,
    }

    impl Grid {
        pub fn flat() -> Self {
            Self {
                blocks: HashMap::new(),
                floor: 63,
            }
        }
        pub fn set(&mut self, p: BlockPos, s: Shape) {
            let f = if s.is_empty() {
                BlockFlags::NONE
            } else {
                BlockFlags::SOLID
            };
            self.blocks.insert(p, (s, f));
        }
    }

    impl PathWorld for Grid {
        fn shape(&self, p: BlockPos) -> Option<Shape> {
            if let Some(b) = self.blocks.get(&p) {
                return Some(b.0);
            }
            Some(if p.y <= self.floor {
                Shape::FULL
            } else {
                Shape::Empty
            })
        }
        fn flags(&self, p: BlockPos) -> BlockFlags {
            self.blocks.get(&p).map(|b| b.1).unwrap_or(BlockFlags::NONE)
        }
        fn dig_ticks(&self, _p: BlockPos) -> Option<u32> {
            Some(10)
        }
    }

    #[test]
    fn straight_line_on_flat_ground() {
        let w = Grid::flat();
        let r = find_path(
            &w,
            BlockPos::new(0, 64, 0),
            &GoalBlock::new(10, 64, 0),
            &PathOptions::default(),
        );
        assert_eq!(r.status, PathStatus::Complete);
        assert_eq!(r.steps.len(), 10);
        assert_eq!(r.steps.last().unwrap().pos, BlockPos::new(10, 64, 0));
    }

    #[test]
    fn climbs_a_step_and_drops_down() {
        let mut w = Grid::flat();
        for z in -3..=3 {
            w.set(BlockPos::new(3, 64, z), Shape::FULL);
        }
        let opt = PathOptions {
            allow_dig: false,
            ..Default::default()
        };
        let r = find_path(&w, BlockPos::new(0, 64, 0), &GoalBlock::new(6, 64, 0), &opt);
        assert_eq!(r.status, PathStatus::Complete);
        let kinds: Vec<_> = r.steps.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&MoveKind::Ascend));
        assert!(kinds.contains(&MoveKind::Descend));
    }

    #[test]
    fn parkours_over_gap() {
        let mut w = Grid::flat();
        for x in -5..=5 {
            for y in 50..=63 {
                w.set(BlockPos::new(x, y, 3), Shape::Empty);
                w.set(BlockPos::new(x, y, 4), Shape::Empty);
            }
        }
        let opt = PathOptions {
            allow_dig: false,
            ..Default::default()
        };
        let r = find_path(&w, BlockPos::new(0, 64, 0), &GoalBlock::new(0, 64, 7), &opt);
        assert_eq!(r.status, PathStatus::Complete);
        assert!(r.steps.iter().any(|s| s.kind == MoveKind::Parkour));
    }

    #[test]
    fn digs_through_wall_when_allowed() {
        let mut w = Grid::flat();
        for z in -20..=20 {
            for y in 64..=70 {
                w.set(BlockPos::new(3, y, z), Shape::FULL);
            }
        }
        let r = find_path(
            &w,
            BlockPos::new(0, 64, 0),
            &GoalBlock::new(6, 64, 0),
            &PathOptions::default(),
        );
        assert_eq!(r.status, PathStatus::Complete);
        assert!(r.steps.iter().any(|s| s.dig.iter().any(Option::is_some)));

        let opt = PathOptions {
            allow_dig: false,
            max_nodes: 2000,
            ..Default::default()
        };
        let r = find_path(&w, BlockPos::new(0, 64, 0), &GoalBlock::new(6, 64, 0), &opt);
        assert!(!r.steps.iter().any(|s| s.dig.iter().any(Option::is_some)));
    }

    #[test]
    fn bridges_with_scaffold_blocks() {
        let mut w = Grid::flat();
        for x in -30..=30 {
            for z in 3..=8 {
                for y in 0..=63 {
                    w.set(BlockPos::new(x, y, z), Shape::Empty);
                }
            }
        }
        let opt = PathOptions {
            allow_dig: false,
            allow_parkour: false,
            scaffold_blocks: 16,
            ..Default::default()
        };
        let r = find_path(
            &w,
            BlockPos::new(0, 64, 0),
            &GoalNear::new(BlockPos::new(0, 64, 10), 0.0),
            &opt,
        );
        assert_eq!(r.status, PathStatus::Complete);
        assert!(
            r.steps
                .iter()
                .filter(|s| s.kind == MoveKind::Bridge)
                .count()
                >= 6
        );
    }
}
