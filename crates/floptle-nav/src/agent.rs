//! Characters that walk the navmesh — the half that was missing.
//!
//! Everything else in this crate answers questions: where can I stand, how do I
//! get there, how far is it. None of that walks anybody anywhere. A game asking
//! for a list of corners still has to hold on to it, notice when it is stale,
//! step along it at the right speed, slow down at the end, not shove its friends
//! off a cliff, and start again when the door it was heading for shuts. That is
//! several hundred lines of the same code in every project, and getting it
//! subtly wrong is what makes AI look broken.
//!
//! So this is the other half. An [`Agent`] is a thing with a position that has
//! somewhere to be, and a [`Crowd`] is all of them together:
//!
//! ```
//! use floptle_nav::{bake, AgentParams, Crowd, NavSettings, Tri};
//! # let floor = [
//! #     Tri::new([0.0, 0.0, 0.0], [12.0, 0.0, 0.0], [0.0, 0.0, 12.0]),
//! #     Tri::new([12.0, 0.0, 0.0], [12.0, 0.0, 12.0], [0.0, 0.0, 12.0]),
//! # ];
//! let mesh = bake(&floor, &NavSettings::default()).unwrap();
//! let mut crowd = Crowd::default();
//!
//! let unit = crowd.add(AgentParams::default(), [2.0, 0.0, 2.0]);
//! crowd.agent_mut(unit).unwrap().move_to([10.0, 0.0, 10.0]);
//!
//! for _ in 0..600 {
//!     crowd.step(Some(&mesh), 1.0 / 60.0);
//! }
//! assert!(crowd.agent(unit).unwrap().arrived());
//! ```
//!
//! # What an agent is responsible for
//!
//! Its **position**, and nothing else. It does not know what a node is, whether
//! there is a rigidbody involved, or what animation should play. Whoever owns
//! the agent reads [`Agent::pos`] and [`Agent::vel`] and decides what those mean
//! — drive a transform, feed a physics body, or drive nothing at all and just
//! watch. That is what keeps this file testable without a scene in it, and it is
//! what stops the engine deciding on a developer's behalf that a navmesh agent
//! must be a particular kind of object.
//!
//! # Following, not marching
//!
//! The path is a list of corners, but walking corner to corner is what makes a
//! character look like it is on rails. Each step the agent tries to see the
//! corner **after** the one it is heading for ([`NavMesh::raycast`]), and skips
//! ahead when it can. The result is that a path bends where the level bends and
//! straightens everywhere else, and it keeps doing that as the agent is pushed
//! around by its neighbours.
//!
//! # Avoidance is a sampling problem here
//!
//! Given a velocity it would like, an agent tries that one and a fan of
//! alternatives, and scores each by how soon it would run into somebody and how
//! far it is from what was wanted. It is a velocity-obstacle method by another
//! name, and it is deliberately not ORCA: ORCA is exact for the linear program
//! it solves and quite hard to reason about when it deadlocks, while a fan of
//! samples degrades into "everybody slows down and shuffles", which is what a
//! crowd of units in a doorway should look like anyway.
//!
//! # Budget
//!
//! A search is cheap and two hundred searches in one frame is not. Agents queue
//! for a path and [`Crowd::paths_per_step`] of them are served each step, oldest
//! wait first, so a hundred units given the same order at the same moment spread
//! their thinking over a few frames instead of dropping one. Nothing stalls
//! while it waits — an agent with an old path keeps walking it.

use std::collections::VecDeque;

use crate::filter::QueryFilter;
use crate::mesh::{dist, NavMesh};
use crate::path::Crossing;

/// A handle to one agent in a [`Crowd`].
///
/// Carries the slot **and** a generation, so a handle to a removed agent is
/// detectably stale rather than quietly pointing at whoever moved in
/// afterwards. That is the same salt Recast puts in a polygon reference, for the
/// same reason: the alternative is an order arriving at the wrong unit, months
/// later, once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentId {
    slot: u32,
    generation: u32,
}

/// What an agent is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// No order. Standing still, but still solid enough that others go round it.
    Idle,
    /// Walking somewhere.
    Moving,
    /// Got there.
    Arrived,
    /// It cannot get there right now, and it has stopped trying. Either the
    /// goal was never reachable, or it has made no progress for
    /// [`AgentParams::stuck_after`] seconds. **A state, not a silence** — the
    /// whole reason it exists is that a unit standing still with no explanation
    /// is the single most common "the pathfinding is broken" report there is.
    ///
    /// Not always terminal: a block whose route was viable (a crowd pin at a
    /// doorway) rests and retries on its own; one whose route never reached
    /// stands until the navmesh changes or a new order arrives.
    Blocked,
    /// Crossing a link: on a ladder, mid-jump, going through a door. See
    /// [`Agent::crossing`] for which one and how far along.
    Crossing,
}

/// How one character walks.
///
/// The defaults describe a person-sized unit on a metres-and-seconds scale — the
/// same character [`NavSettings`](crate::NavSettings) defaults to, so an agent
/// made with no configuration at all fits the mesh baked with no configuration
/// at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentParams {
    /// How wide it is, for keeping out of its neighbours' way. Separate from the
    /// bake's `agent_radius`, which is about walls: two units of different sizes
    /// can share one navmesh and still not stand inside each other.
    pub radius: f32,
    /// Top speed, in units per second.
    pub speed: f32,
    /// How hard it gets up to speed and back down. High is crisp; low has
    /// visible momentum.
    pub accel: f32,
    /// Close enough to the order to call it arrived. Keep it at least the
    /// radius, or a group ordered to one spot jostles forever trying to stand on
    /// it.
    pub arrive: f32,
    /// Start slowing down this far out. 0 means stop dead on arrival.
    pub slow: f32,
    /// Take other agents into account. Off is right for something that should
    /// walk its line and let the others sort themselves out — a boss, a vehicle,
    /// a scripted march.
    pub avoid: bool,
    /// Who gives way. An agent yields to anything of higher priority and expects
    /// anything lower to yield to it; equal priorities split the difference.
    pub priority: f32,
    /// How hard it pushes out of an overlap, 0 to 1. This is what stops a group
    /// ordered onto one point from becoming one unit.
    pub separation: f32,
    /// How often to check that the route is still the right one, in seconds. The
    /// check is cheap and the search behind it is budgeted, so this is a knob for
    /// how quickly a unit notices a door opening rather than a performance one.
    pub repath: f32,
    /// Give up after this long without getting measurably closer. 0 never gives
    /// up, which means a unit shoved into a corner pushes at it forever.
    pub stuck_after: f32,
    /// What this character will and will not walk on.
    pub filter: QueryFilter,
}

impl Default for AgentParams {
    fn default() -> Self {
        AgentParams {
            radius: 0.5,
            speed: 3.5,
            accel: 12.0,
            arrive: 0.5,
            slow: 1.5,
            avoid: true,
            priority: 0.5,
            separation: 1.0,
            repath: 0.75,
            stuck_after: 4.0,
            filter: QueryFilter::default(),
        }
    }
}

/// A link being crossed right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ride {
    /// Which link, by its id.
    pub link: u32,
    /// Whether it is being crossed the way it was drawn.
    pub forwards: bool,
    /// How far along, 0 to 1 — what an animation should be driven by.
    pub progress: f32,
    pub from: [f32; 3],
    pub to: [f32; 3],
    /// How long the whole crossing takes — the link's own duration when it
    /// names one, else the walk it would have been. Fixed when the ride
    /// starts, so `progress` advances at one honest rate.
    pub seconds: f32,
}

/// One character that walks.
#[derive(Clone, Debug)]
pub struct Agent {
    /// Where it is. Written by the crowd, and safe to write from outside when
    /// something else owns the movement (a physics body, a cutscene).
    pub pos: [f32; 3],
    /// How fast it is going, as of the last step.
    pub vel: [f32; 3],
    pub params: AgentParams,
    state: AgentState,
    target: Option<[f32; 3]>,
    /// The corners left to walk, nearest first, and where the crossings are in
    /// them.
    path: Vec<[f32; 3]>,
    crossings: Vec<Crossing>,
    /// Whether the path in hand reaches the target or only gets near it.
    complete: bool,
    ride: Option<Ride>,
    wants_path: bool,
    since_repath: f32,
    /// The closest this agent has been to its target, and for how long it has
    /// failed to beat that. The progress watchdog.
    best: f32,
    stalled: f32,
    /// Corner-cut throttle: the sight-line raycast is by far the priciest thing
    /// an agent does per step, and its answer barely changes frame to frame —
    /// so it runs a few times a second, phase-scattered across the crowd.
    since_cut: f32,
    /// Last step's avoidance chose to hang back for the crowd. Queueing at a
    /// doorway is progress that hasn't happened YET, not a unit that is stuck —
    /// the watchdog counts it at a fraction of the rate.
    yielding: bool,
}

impl Agent {
    fn new(params: AgentParams, pos: [f32; 3]) -> Agent {
        Agent {
            pos,
            vel: [0.0; 3],
            params,
            state: AgentState::Idle,
            target: None,
            path: Vec::new(),
            crossings: Vec::new(),
            complete: true,
            ride: None,
            wants_path: false,
            since_repath: 0.0,
            best: f32::INFINITY,
            stalled: 0.0,
            since_cut: 0.0,
            yielding: false,
        }
    }

    /// Send it somewhere. Re-ordering it to the same place it is already heading
    /// costs nothing, so calling this every frame from a script is fine.
    pub fn move_to(&mut self, target: [f32; 3]) {
        if let Some(t) = self.target
            && dist(t, target) < 1e-4
            && self.state != AgentState::Blocked
        {
            return;
        }
        self.target = Some(target);
        self.state = AgentState::Moving;
        self.wants_path = true;
        self.since_repath = 0.0;
        self.best = f32::INFINITY;
        self.stalled = 0.0;
    }

    /// Keep the target in step WITHOUT treating it as a fresh order: an
    /// unchanged target is left entirely alone — a Blocked agent stays resting
    /// instead of being woken (and re-queued) every frame by a host that
    /// mirrors world-space targets each step. A changed one — a real order, or
    /// a rebake moving the mesh anchor under the world→local conversion — is
    /// an ordinary [`Agent::move_to`].
    pub fn sync_target(&mut self, target: [f32; 3]) {
        match self.target {
            Some(t) if dist(t, target) < 1e-4 => {}
            _ => self.move_to(target),
        }
    }

    /// Cancel the order and stand still. Anything mid-crossing finishes the
    /// crossing first — stopping halfway up a ladder is not a place to be.
    pub fn stop(&mut self) {
        self.target = None;
        self.path.clear();
        self.crossings.clear();
        self.wants_path = false;
        if self.ride.is_none() {
            self.state = AgentState::Idle;
        }
    }

    /// Put it somewhere else, immediately, and forget what it was doing. For
    /// spawning, teleports, and anything that moves a character without walking.
    pub fn teleport(&mut self, to: [f32; 3]) {
        self.pos = to;
        self.vel = [0.0; 3];
        self.ride = None;
        self.path.clear();
        self.crossings.clear();
        if self.target.is_some() {
            self.wants_path = true;
        }
    }

    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn arrived(&self) -> bool {
        self.state == AgentState::Arrived
    }

    /// Where it was told to go, if anywhere.
    pub fn target(&self) -> Option<[f32; 3]> {
        self.target
    }

    /// Whether the route it is walking actually reaches the target. False means
    /// it is heading for the closest it can get — which is the right thing to do
    /// and worth being able to say out loud.
    pub fn route_complete(&self) -> bool {
        self.complete
    }

    /// The corners still to walk. Empty when there is no order, and the thing to
    /// draw when somebody asks why a unit went that way.
    pub fn remaining(&self) -> &[[f32; 3]] {
        &self.path
    }

    /// How far there is left to walk, in metres, along the route rather than
    /// through the walls.
    pub fn distance_left(&self) -> f32 {
        let mut total = 0.0;
        let mut at = self.pos;
        for p in &self.path {
            total += dist(at, *p);
            at = *p;
        }
        total
    }

    /// The link being crossed right now, if any.
    pub fn crossing(&self) -> Option<Ride> {
        self.ride
    }

    /// Whether this agent has a route in hand yet. False for the frame or two
    /// between an order and the search that answers it.
    pub fn has_path(&self) -> bool {
        !self.path.is_empty()
    }
}

/// Every agent, and the machinery they share.
#[derive(Debug)]
pub struct Crowd {
    slots: Vec<Option<Agent>>,
    generations: Vec<u32>,
    free: Vec<u32>,
    /// Agents waiting for a search, oldest first.
    queue: VecDeque<u32>,
    /// Whether a slot is already in `queue` — an O(1) membership answer, so a
    /// rebake (which re-queues everyone at once) does not become a scan of the
    /// queue per agent per frame while the budget drains it.
    queued: Vec<bool>,
    /// How many searches one step may run. Raise it for a level where orders
    /// arrive in bursts; lower it if a frame spike shows up in profiling.
    pub paths_per_step: usize,
    /// Bumped when the navmesh underneath changes, which makes every path in
    /// flight stale. See [`Crowd::navmesh_changed`].
    revision: u64,
    seen_revision: Vec<u64>,
    /// Scratch, reused every step so a crowd of hundreds does not allocate per
    /// agent per frame.
    neighbours: Vec<Neighbour>,
    /// Plan-space buckets over `neighbours` (indices), rebuilt each step —
    /// avoidance and separation ask "who is near me", and answering that with
    /// the whole crowd is quadratic in the ARMY, the same silent-quadratic
    /// shape [`crate::index`] exists to kill one layer down.
    grid: std::collections::HashMap<(i32, i32), Vec<u32>>,
    /// The fattest agent this step, so a grid query knows how far "near" reaches.
    max_radius: f32,
    /// Scratch for `dodge`'s shortlist (indices into `neighbours`).
    near_scratch: Vec<u32>,
}

/// Neighbour-grid bucket size, metres. Big enough that a query touches a few
/// buckets; small enough that a bucket holds a handful of a packed crowd.
const NEIGHBOUR_CELL: f32 = 4.0;

fn grid_key(pos: [f32; 3]) -> (i32, i32) {
    ((pos[0] / NEIGHBOUR_CELL).floor() as i32, (pos[2] / NEIGHBOUR_CELL).floor() as i32)
}

/// One agent as its neighbours see it: where it is, where it is going, how big
/// it is and how much it expects to be gone round.
#[derive(Clone, Copy, Debug)]
struct Neighbour {
    slot: u32,
    pos: [f32; 3],
    vel: [f32; 3],
    radius: f32,
    priority: f32,
    /// Where this one has ARRIVED, if it has — what makes arrival contagious
    /// (see `advance`): the last of sixty units sent to one spot can never
    /// stand on the spot itself, because fifty-nine friends already do.
    arrived_target: Option<[f32; 3]>,
}

impl Default for Crowd {
    fn default() -> Self {
        Crowd {
            slots: Vec::new(),
            generations: Vec::new(),
            free: Vec::new(),
            queue: VecDeque::new(),
            queued: Vec::new(),
            paths_per_step: 8,
            revision: 0,
            seen_revision: Vec::new(),
            neighbours: Vec::new(),
            grid: std::collections::HashMap::new(),
            max_radius: 0.0,
            near_scratch: Vec::new(),
        }
    }
}

impl Crowd {
    /// Add an agent at a position, and get the handle to order it about with.
    pub fn add(&mut self, params: AgentParams, pos: [f32; 3]) -> AgentId {
        let mut agent = Agent::new(params, pos);
        let id = match self.free.pop() {
            Some(slot) => AgentId { slot, generation: self.generations[slot as usize] },
            None => {
                self.slots.push(None);
                self.generations.push(0);
                self.seen_revision.push(self.revision);
                self.queued.push(false);
                AgentId { slot: (self.slots.len() - 1) as u32, generation: 0 }
            }
        };
        // Scatter the corner-cut phase so a crowd created together does not run
        // every sight-line check on the same frame forever after.
        agent.since_cut = (id.slot as f32 * 0.037) % 0.15;
        self.seen_revision[id.slot as usize] = self.revision;
        self.queued[id.slot as usize] = false;
        self.slots[id.slot as usize] = Some(agent);
        id
    }

    /// Take one out. The handle is stale afterwards and every later call with it
    /// answers `None` rather than reaching whoever takes the slot next.
    pub fn remove(&mut self, id: AgentId) {
        if !self.alive(id) {
            return;
        }
        self.slots[id.slot as usize] = None;
        self.generations[id.slot as usize] = self.generations[id.slot as usize].wrapping_add(1);
        self.free.push(id.slot);
    }

    pub fn alive(&self, id: AgentId) -> bool {
        self.generations.get(id.slot as usize) == Some(&id.generation)
            && self.slots.get(id.slot as usize).is_some_and(|s| s.is_some())
    }

    pub fn agent(&self, id: AgentId) -> Option<&Agent> {
        if !self.alive(id) {
            return None;
        }
        self.slots[id.slot as usize].as_ref()
    }

    pub fn agent_mut(&mut self, id: AgentId) -> Option<&mut Agent> {
        if !self.alive(id) {
            return None;
        }
        self.slots[id.slot as usize].as_mut()
    }

    /// How many agents there are.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| s.is_none())
    }

    /// Every live agent and its handle.
    pub fn iter(&self) -> impl Iterator<Item = (AgentId, &Agent)> {
        self.slots.iter().enumerate().filter_map(move |(i, a)| {
            a.as_ref().map(|a| (AgentId { slot: i as u32, generation: self.generations[i] }, a))
        })
    }

    /// Throw everybody out — a scene change, or Stop in the editor.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.generations.clear();
        self.free.clear();
        self.queue.clear();
        self.queued.clear();
        self.seen_revision.clear();
        self.grid.clear();
    }

    /// The navmesh has been rebaked, or a link has opened or closed.
    ///
    /// Every route in flight was worked out against the old one, so they are all
    /// suspect: each agent re-asks on its next step, spread over the usual
    /// budget. Nobody stops walking in the meantime — an old path through a
    /// level that mostly did not change is a better guess than standing still.
    pub fn navmesh_changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Step every agent forward by `dt`.
    ///
    /// With no navmesh, agents hold still: an order given before the bake exists
    /// is remembered and acted on when it does.
    pub fn step(&mut self, mesh: Option<&NavMesh>, dt: f32) {
        let dt = dt.clamp(0.0, 0.25);
        if dt <= 0.0 {
            return;
        }
        let Some(mesh) = mesh else { return };

        self.expire_stale_paths();
        self.serve_paths(mesh);
        self.collect_neighbours();

        for slot in 0..self.slots.len() {
            if self.slots[slot].is_none() {
                continue;
            }
            // Taken out of its slot for the step so the agent can be advanced
            // while its neighbours are read. Put back below, always.
            let mut agent = self.slots[slot].take().unwrap();
            self.advance(&mut agent, slot as u32, mesh, dt);
            self.slots[slot] = Some(agent);
        }
    }

    /// Agents whose route predates the last change to the navmesh ask again.
    fn expire_stale_paths(&mut self) {
        for slot in 0..self.slots.len() {
            if self.seen_revision[slot] == self.revision {
                continue;
            }
            self.seen_revision[slot] = self.revision;
            if let Some(a) = self.slots[slot].as_mut()
                && a.target.is_some()
            {
                a.wants_path = true;
                // A door that opened may have made a hopeless order possible,
                // so a blocked agent gets to try again too.
                if a.state == AgentState::Blocked {
                    a.state = AgentState::Moving;
                    a.best = f32::INFINITY;
                    a.stalled = 0.0;
                }
            }
        }
    }

    /// Run up to `paths_per_step` searches for the agents that asked, oldest
    /// first.
    fn serve_paths(&mut self, mesh: &NavMesh) {
        for slot in 0..self.slots.len() {
            // An agent mid-crossing is off the mesh by definition — a search
            // asked from halfway up a ladder fails and poisons its state. It
            // keeps wanting; it queues once it lands.
            let wants =
                self.slots[slot].as_ref().is_some_and(|a| a.wants_path && a.ride.is_none());
            if wants && !self.queued[slot] {
                self.queue.push_back(slot as u32);
                self.queued[slot] = true;
            }
        }
        let mut served = 0;
        while served < self.paths_per_step {
            let Some(slot) = self.queue.pop_front() else { break };
            self.queued[slot as usize] = false;
            let Some(agent) = self.slots.get_mut(slot as usize).and_then(|s| s.as_mut()) else {
                continue;
            };
            if !agent.wants_path {
                continue;
            }
            agent.wants_path = false;
            served += 1;
            let Some(target) = agent.target else { continue };
            let snap = mesh.settings.agent_height.max(agent.params.radius).max(0.5);
            match mesh.path_with(agent.pos, target, snap, &agent.params.filter) {
                Some(path) => {
                    agent.complete = path.complete;
                    agent.crossings = path.crossings;
                    agent.path = path.points;
                    // The first point is where the agent already is, so it is
                    // dropped — unless a crossing starts there, which is what a
                    // route asked for while standing on a link's mouth looks
                    // like. Dropping that one would leave the walk heading
                    // straight for the far end through whatever is in between.
                    if !agent.path.is_empty() && !agent.crossings.iter().any(|c| c.at == 0) {
                        agent.path.remove(0);
                        for c in &mut agent.crossings {
                            c.at -= 1;
                        }
                    }
                    if agent.path.is_empty() {
                        agent.state = AgentState::Arrived;
                    } else if agent.state != AgentState::Crossing {
                        agent.state = AgentState::Moving;
                    }
                }
                None => {
                    // Neither end is on the navmesh at all: nothing to walk, and
                    // saying so beats walking towards it in a straight line
                    // through the scenery.
                    agent.path.clear();
                    agent.crossings.clear();
                    agent.complete = false;
                    if agent.ride.is_none() {
                        agent.state = AgentState::Blocked;
                    }
                }
            }
        }
    }

    /// Snapshot everybody's position and size for this step's avoidance, and
    /// bucket them so "who is near me" is answered by locality, not a scan.
    fn collect_neighbours(&mut self) {
        self.neighbours.clear();
        for v in self.grid.values_mut() {
            v.clear();
        }
        self.max_radius = 0.0;
        for (slot, a) in self.slots.iter().enumerate() {
            let Some(a) = a else { continue };
            let i = self.neighbours.len() as u32;
            self.neighbours.push(Neighbour {
                slot: slot as u32,
                pos: a.pos,
                vel: a.vel,
                radius: a.params.radius,
                priority: a.params.priority,
                arrived_target: (a.state == AgentState::Arrived)
                    .then_some(a.target)
                    .flatten(),
            });
            self.max_radius = self.max_radius.max(a.params.radius);
            self.grid.entry(grid_key(a.pos)).or_default().push(i);
        }
        // Cells the crowd has walked away from would otherwise pile up forever.
        self.grid.retain(|_, v| !v.is_empty());
    }

    /// Every neighbour index within `reach` of `pos` (plus whatever the buckets
    /// over-report — callers still distance-test, exactly like the poly index).
    fn for_each_near(&self, pos: [f32; 3], reach: f32, mut f: impl FnMut(u32)) {
        let r = reach.max(0.0);
        let (x0, z0) = grid_key([pos[0] - r, 0.0, pos[2] - r]);
        let (x1, z1) = grid_key([pos[0] + r, 0.0, pos[2] + r]);
        for gz in z0..=z1 {
            for gx in x0..=x1 {
                if let Some(cell) = self.grid.get(&(gx, gz)) {
                    for &i in cell {
                        f(i);
                    }
                }
            }
        }
    }

    fn advance(&mut self, agent: &mut Agent, slot: u32, mesh: &NavMesh, dt: f32) {
        agent.since_repath += dt;

        if agent.ride.is_some() {
            self.ride_link(agent, dt);
            return;
        }

        // Somewhere to be?
        let Some(target) = agent.target else {
            agent.vel = damp(agent.vel, agent.params.accel, dt);
            self.settle(agent, slot, mesh, dt);
            return;
        };

        if agent.state == AgentState::Blocked {
            agent.vel = damp(agent.vel, agent.params.accel, dt);
            self.settle(agent, slot, mesh, dt);
            // A watchdog block WITH a viable route is usually a crowd pin — a
            // unit shoved against a door jamb by sixty friends — not a dead
            // end. It rests, then tries again; standing forever in a doorway
            // that cleared ten seconds ago is the worse behaviour. A route
            // that never reached (`complete == false`) stays blocked until
            // the navmesh changes or a new order arrives.
            if agent.complete && agent.target.is_some() {
                agent.stalled += dt;
                if agent.stalled > agent.params.stuck_after.max(0.5) * 2.0 {
                    agent.state = AgentState::Moving;
                    agent.wants_path = true;
                    agent.best = f32::INFINITY;
                    agent.stalled = 0.0;
                }
            }
            return;
        }

        self.follow_corridor(agent, mesh, dt);

        // An order the search has not answered yet is not an arrival and not a
        // walk — there is no route to judge either against. Hold position for
        // the frame or two until the budget serves it. (Without this, a fresh
        // order reads as `arrived` for a frame, because an unserved agent has
        // an empty path and `complete` defaults true.)
        if agent.path.is_empty() && agent.wants_path {
            agent.vel = damp(agent.vel, agent.params.accel, dt);
            self.settle(agent, slot, mesh, dt);
            return;
        }

        // Arrival is contagious on the last leg: touching somebody who has
        // already arrived at (near enough) the same order counts. Without it,
        // the crowd's ring around a shared destination excludes the point
        // itself and the last few units grind at their friends forever.
        if agent.path.len() <= 1 {
            let mut settled = false;
            self.for_each_near(agent.pos, agent.params.radius + self.max_radius + 0.2, |i| {
                let n = &self.neighbours[i as usize];
                if settled || n.slot == slot {
                    return;
                }
                if let Some(t) = n.arrived_target
                    && flat_dist(t, target) <= agent.params.arrive.max(1.0)
                    && flat_dist(agent.pos, n.pos) <= agent.params.radius + n.radius + 0.1
                {
                    settled = true;
                }
            });
            if settled {
                agent.state = AgentState::Arrived;
                agent.path.clear();
                agent.crossings.clear();
                agent.wants_path = false; // the order is answered: it is here
                agent.vel = damp(agent.vel, agent.params.accel, dt);
                self.settle(agent, slot, mesh, dt);
                return;
            }
        }

        // Arrived?
        let left = flat_dist(agent.pos, target);
        if agent.path.is_empty() || (agent.path.len() == 1 && left <= agent.params.arrive) {
            if left <= agent.params.arrive || (agent.path.is_empty() && agent.complete) {
                agent.state = AgentState::Arrived;
                agent.path.clear();
                agent.crossings.clear();
                agent.vel = damp(agent.vel, agent.params.accel, dt);
                self.settle(agent, slot, mesh, dt);
                return;
            }
            if agent.path.is_empty() {
                // As near as the ground allows, and no nearer.
                agent.state =
                    if agent.complete { AgentState::Arrived } else { AgentState::Blocked };
                agent.vel = damp(agent.vel, agent.params.accel, dt);
                self.settle(agent, slot, mesh, dt);
                return;
            }
        }

        // The watchdog: measured against the route, not the straight line, so a
        // unit walking the long way round a wall is making progress.
        let progress = agent.distance_left();
        if progress < agent.best - 0.05 {
            agent.best = progress;
            agent.stalled = 0.0;
        } else if agent.params.stuck_after > 0.0 {
            // Waiting one's turn in a crowd is not being stuck: while the
            // avoidance is deliberately hanging back, the clock runs slow —
            // a genuine gridlock still ends as Blocked, just not before the
            // queue in front has had a fair chance to clear.
            agent.stalled += if agent.yielding { dt * 0.25 } else { dt };
            if agent.stalled > agent.params.stuck_after {
                agent.state = AgentState::Blocked;
                agent.vel = [0.0; 3];
                return;
            }
        }

        // Ask again now and then, so an opened door or a rebake is noticed.
        if agent.params.repath > 0.0 && agent.since_repath >= agent.params.repath {
            agent.since_repath = 0.0;
            // Only when it might change the answer: a complete route that is
            // being walked is not improved by asking for it again.
            if !agent.complete || agent.path.is_empty() {
                agent.wants_path = true;
            }
        }

        let next = agent.path[0];
        let free = self.desired(agent, next, target);
        // How far ahead to fear a collision. Mid-route, two seconds. On the
        // last leg it shrinks with the distance left, or a unit approaching a
        // settled crowd freezes at its own fear radius and can never make the
        // CONTACT that counts as arriving — separation owns the last metre.
        let final_leg = agent.path.len() <= 1;
        let horizon = if final_leg {
            ((left / agent.params.speed.max(0.1)) * 0.9).clamp(0.25, 2.0)
        } else {
            2.0
        };
        let want = if agent.params.avoid {
            self.dodge(agent, slot, free, horizon, final_leg)
        } else {
            free
        };
        // Materially slower than it wanted to go = hanging back for the crowd.
        let sq = |v: [f32; 3]| v[0] * v[0] + v[2] * v[2];
        agent.yielding = agent.params.avoid && sq(want) < sq(free) * 0.25;
        agent.vel = ease(agent.vel, want, agent.params.accel, dt);
        agent.pos = [
            agent.pos[0] + agent.vel[0] * dt,
            agent.pos[1] + agent.vel[1] * dt,
            agent.pos[2] + agent.vel[2] * dt,
        ];
        self.settle(agent, slot, mesh, dt);
        self.maybe_start_ride(agent, mesh);
    }

    /// Drop corners already passed, and cut ahead to the furthest one in plain
    /// sight — the continuous half of corridor following.
    fn follow_corridor(&self, agent: &mut Agent, mesh: &NavMesh, dt: f32) {
        let reach = (agent.params.radius * 0.5).max(mesh.cell_size).max(0.1);
        while agent.path.len() > 1 {
            // Never step over a crossing: its mouth is where the ladder is.
            if agent.crossings.iter().any(|c| c.at == 0) {
                break;
            }
            if flat_dist(agent.pos, agent.path[0]) > reach {
                break;
            }
            agent.path.remove(0);
            agent.crossings.retain(|c| c.at > 0);
            for c in &mut agent.crossings {
                c.at -= 1;
            }
        }

        // Corner cutting: if the corner after next can be walked to directly,
        // the one in between was a detour round a shape this agent is not
        // actually near. The sight-line march is the priciest thing an agent
        // does per step and its answer barely changes frame to frame, so it
        // runs a few times a second (phase-scattered per agent — see `add`),
        // not every frame for every agent.
        agent.since_cut = (agent.since_cut + dt).min(1.0);
        if agent.since_cut >= 0.15
            && agent.path.len() > 1
            && !agent.crossings.iter().any(|c| c.at <= 1)
        {
            agent.since_cut = 0.0;
            if mesh
                .raycast_with(
                    agent.pos,
                    agent.path[1],
                    mesh.settings.agent_height.max(0.5),
                    &agent.params.filter,
                )
                .is_none()
            {
                agent.path.remove(0);
                agent.crossings.retain(|c| c.at > 0);
                for c in &mut agent.crossings {
                    c.at -= 1;
                }
            }
        }
    }

    /// The velocity this agent would pick with the level to itself.
    fn desired(&self, agent: &Agent, next: [f32; 3], target: [f32; 3]) -> [f32; 3] {
        let mut dir = [next[0] - agent.pos[0], 0.0, next[2] - agent.pos[2]];
        let len = (dir[0] * dir[0] + dir[2] * dir[2]).sqrt();
        if len <= 1e-5 {
            return [0.0; 3];
        }
        dir[0] /= len;
        dir[2] /= len;

        // Ease off near the end of the whole route, not near every corner.
        let mut speed = agent.params.speed;
        if agent.params.slow > 0.0 && agent.path.len() <= 1 {
            let left = flat_dist(agent.pos, target);
            let t = (left / agent.params.slow).clamp(0.0, 1.0);
            speed *= t.max(0.05);
        }
        [dir[0] * speed, 0.0, dir[2] * speed]
    }

    /// Pick a velocity near the one wanted that runs into the fewest people —
    /// fearing only collisions within `horizon` seconds. On the `final_leg`,
    /// neighbours already at contact range stop counting: the last metre
    /// belongs to separation, or a unit can never join the crowd it is
    /// arriving into.
    fn dodge(
        &mut self,
        agent: &Agent,
        slot: u32,
        want: [f32; 3],
        horizon: f32,
        final_leg: bool,
    ) -> [f32; 3] {
        let range = agent.params.radius * 6.0;
        // The shortlist comes from the grid (locality, not the whole army) and
        // lives in a reused scratch, so a big crowd neither scans nor allocates
        // here — put back below, always.
        let mut near = std::mem::take(&mut self.near_scratch);
        near.clear();
        self.for_each_near(agent.pos, range + self.max_radius, |i| {
            let n = &self.neighbours[i as usize];
            if n.slot != slot && flat_dist(agent.pos, n.pos) < range + n.radius {
                near.push(i);
            }
        });
        if near.is_empty() {
            self.near_scratch = near;
            return want;
        }

        let score = |v: [f32; 3]| -> f32 {
            let mut worst = 0.0f32;
            for &i in &near {
                let n = &self.neighbours[i as usize];
                // On the last leg, somebody already at (or nearly at) contact
                // range is separation's problem, not a collision to fear —
                // fearing them is how two arriving units freeze a hand-span
                // apart forever, each inside the other's collision cone and
                // stop the cheapest answer for both. Mid-route the fear stays:
                // it is what queues a crowd politely at a doorway.
                if final_leg
                    && flat_dist(agent.pos, n.pos) <= agent.params.radius + n.radius + 0.15
                {
                    continue;
                }
                // How responsible this agent is for the dodge. Equal priorities
                // split it; something more important expects to be gone round.
                let share = (0.5 + (n.priority - agent.params.priority)).clamp(0.0, 1.0);
                if share <= 0.0 {
                    continue;
                }
                let t =
                    time_to_hit(agent.pos, v, n.pos, n.vel, agent.params.radius + n.radius);
                if let Some(t) = t {
                    // Anything beyond the horizon is not worth steering for —
                    // it will have moved by then.
                    if t < horizon {
                        worst = worst.max(share * (horizon - t) / (t + 0.1));
                    }
                }
            }
            let dv = [v[0] - want[0], v[2] - want[2]];
            let deviation = (dv[0] * dv[0] + dv[1] * dv[1]).sqrt();
            worst * 3.0 + deviation
        };

        let mut best = want;
        let mut best_score = score(want);
        // A fan either side, at full speed and at half — so "go round" and "hang
        // back" are both on the table, which is what stops two units meeting
        // head-on from stepping into each other forever.
        for k in 1..=4 {
            let a = k as f32 * 0.35;
            for sign in [1.0f32, -1.0] {
                for scale in [1.0f32, 0.55] {
                    let v = rotate_y(want, a * sign, scale);
                    let s = score(v);
                    if s < best_score {
                        best_score = s;
                        best = v;
                    }
                }
            }
        }
        // Standing still is always an option, and sometimes the right one.
        let stop_score = score([0.0; 3]);
        if stop_score < best_score {
            best = [0.0; 3];
        }
        self.near_scratch = near;
        best
    }

    /// Keep the agent on the navmesh and out of its neighbours.
    ///
    /// Position, not velocity: an overlap that has already happened has to be
    /// undone, and a velocity nudge only stops it getting worse. The push is
    /// applied first and the mesh has the last word, so a shove can never put a
    /// character through a wall — which is the one failure that would make this
    /// whole layer untrustworthy.
    fn settle(&self, agent: &mut Agent, slot: u32, mesh: &NavMesh, dt: f32) {
        if agent.params.separation > 0.0 {
            let mut push = [0.0f32; 3];
            self.for_each_near(agent.pos, agent.params.radius + self.max_radius, |i| {
                let n = &self.neighbours[i as usize];
                // Its own start-of-frame snapshot is not a neighbour — pushing
                // away from it is a shove forward along this frame's motion,
                // i.e. free (and frame-rate-dependent) extra speed.
                if n.slot == slot {
                    return;
                }
                let want = agent.params.radius + n.radius;
                let d = flat_dist(agent.pos, n.pos);
                if d >= want || d <= 1e-5 {
                    return;
                }
                let k = (want - d) / want;
                push[0] += (agent.pos[0] - n.pos[0]) / d * k;
                push[2] += (agent.pos[2] - n.pos[2]) / d * k;
            });
            let strength = agent.params.separation * agent.params.radius * 4.0 * dt;
            agent.pos[0] += push[0] * strength;
            agent.pos[2] += push[2] * strength;
        }

        let snap = mesh.settings.agent_height.max(1.0);
        if let Some((_, on)) = mesh.nearest_with(agent.pos, snap, &agent.params.filter) {
            agent.pos = on;
        }
    }

    /// Step onto a link if the walk has reached one.
    fn maybe_start_ride(&self, agent: &mut Agent, mesh: &NavMesh) {
        let Some(c) = agent.crossings.iter().copied().find(|c| c.at == 0) else { return };
        let Some(mouth) = agent.path.first().copied() else { return };
        if flat_dist(agent.pos, mouth) > agent.params.radius.max(mesh.cell_size) {
            return;
        }
        let Some(link) = mesh.off_links.iter().find(|l| l.id == c.link) else { return };
        if !link.usable(c.forwards) {
            // It shut while we were walking to it. Ask for another way round.
            agent.wants_path = true;
            return;
        }
        let (from, to) = link.ends(c.forwards);
        // A link that names its own duration (a lift, a slow climb) is crossed
        // at that pace; one that does not is crossed at walking speed.
        let seconds = Self::crossing_seconds(link.duration, dist(from, to), agent.params.speed);
        agent.pos = from;
        agent.vel = [0.0; 3];
        agent.ride =
            Some(Ride { link: c.link, forwards: c.forwards, progress: 0.0, from, to, seconds });
        agent.state = AgentState::Crossing;
        // The mouth is walked; what remains is the far side onwards.
        if !agent.path.is_empty() {
            agent.path.remove(0);
        }
        agent.crossings.retain(|x| x.at > 0);
        for x in &mut agent.crossings {
            x.at -= 1;
        }
    }

    /// Carry an agent along a link it is already on.
    fn ride_link(&self, agent: &mut Agent, dt: f32) {
        let Some(mut ride) = agent.ride else { return };
        let seconds = ride.seconds.max(0.05);
        ride.progress = (ride.progress + dt / seconds).min(1.0);
        let t = ride.progress;
        agent.pos = [
            ride.from[0] + (ride.to[0] - ride.from[0]) * t,
            ride.from[1] + (ride.to[1] - ride.from[1]) * t,
            ride.from[2] + (ride.to[2] - ride.from[2]) * t,
        ];
        agent.vel = [
            (ride.to[0] - ride.from[0]) / seconds,
            (ride.to[1] - ride.from[1]) / seconds,
            (ride.to[2] - ride.from[2]) / seconds,
        ];
        if t >= 1.0 {
            agent.pos = ride.to;
            agent.vel = [0.0; 3];
            agent.ride = None;
            agent.state = if agent.target.is_some() && !agent.path.is_empty() {
                AgentState::Moving
            } else if agent.target.is_some() {
                AgentState::Arrived
            } else {
                AgentState::Idle
            };
        } else {
            agent.ride = Some(ride);
        }
    }

    /// How long a crossing takes for this agent, honouring a link that names its
    /// own duration.
    pub fn crossing_seconds(link_duration: f32, span: f32, speed: f32) -> f32 {
        if link_duration > 0.0 {
            link_duration
        } else {
            (span / speed.max(0.01)).max(0.05)
        }
    }
}

fn flat_dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dz) = (a[0] - b[0], a[2] - b[2]);
    (dx * dx + dz * dz).sqrt()
}

/// Frame-rate independent approach — the same at 30 fps and at 240.
fn ease(from: [f32; 3], to: [f32; 3], rate: f32, dt: f32) -> [f32; 3] {
    let k = 1.0 - (-rate.max(0.0) * dt).exp();
    [
        from[0] + (to[0] - from[0]) * k,
        from[1] + (to[1] - from[1]) * k,
        from[2] + (to[2] - from[2]) * k,
    ]
}

fn damp(v: [f32; 3], rate: f32, dt: f32) -> [f32; 3] {
    ease(v, [0.0; 3], rate, dt)
}

fn rotate_y(v: [f32; 3], angle: f32, scale: f32) -> [f32; 3] {
    let (s, c) = angle.sin_cos();
    [(v[0] * c - v[2] * s) * scale, v[1], (v[0] * s + v[2] * c) * scale]
}

/// When two circles moving at constant velocity would touch, if they would.
///
/// The standard velocity-obstacle test: solve `|w + vt| = r` for the smaller
/// positive root. Already overlapping answers 0, which scores as the worst thing
/// that can happen and is what makes a crowd unpack itself.
fn time_to_hit(
    pos: [f32; 3],
    vel: [f32; 3],
    other: [f32; 3],
    other_vel: [f32; 3],
    radius: f32,
) -> Option<f32> {
    let w = [other[0] - pos[0], other[2] - pos[2]];
    let v = [vel[0] - other_vel[0], vel[2] - other_vel[2]];
    let c = w[0] * w[0] + w[1] * w[1] - radius * radius;
    if c <= 0.0 {
        return Some(0.0);
    }
    let a = v[0] * v[0] + v[1] * v[1];
    if a <= 1e-6 {
        return None;
    }
    let b = w[0] * v[0] + w[1] * v[1];
    if b <= 0.0 {
        return None; // moving apart
    }
    let disc = b * b - a * c;
    if disc <= 0.0 {
        return None;
    }
    Some((b - disc.sqrt()) / a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bake, bake_with, NavSettings, OffLink, Tri};

    fn slab(x0: f32, z0: f32, w: f32, d: f32, y: f32) -> Vec<Tri> {
        vec![
            Tri::new([x0, y, z0], [x0 + w, y, z0], [x0, y, z0 + d]),
            Tri::new([x0 + w, y, z0], [x0 + w, y, z0 + d], [x0, y, z0 + d]),
        ]
    }

    fn settings() -> NavSettings {
        NavSettings { agent_radius: 0.3, cell_size: 0.15, ..Default::default() }
    }

    fn run(crowd: &mut Crowd, mesh: &NavMesh, seconds: f32) {
        let dt = 1.0 / 60.0;
        for _ in 0..(seconds / dt) as usize {
            crowd.step(Some(mesh), dt);
        }
    }

    /// The whole point: an order, and it gets there.
    #[test]
    fn an_agent_told_to_go_somewhere_goes_there() {
        let mesh = bake(&slab(0.0, 0.0, 12.0, 12.0, 0.0), &settings()).unwrap();
        let mut crowd = Crowd::default();
        let id = crowd.add(AgentParams::default(), [1.5, 0.0, 1.5]);
        crowd.agent_mut(id).unwrap().move_to([10.0, 0.0, 10.0]);

        run(&mut crowd, &mesh, 12.0);
        let a = crowd.agent(id).unwrap();
        assert_eq!(a.state(), AgentState::Arrived, "at {:?}", a.pos);
        assert!(flat_dist(a.pos, [10.0, 0.0, 10.0]) <= a.params.arrive + 0.1, "{:?}", a.pos);
    }

    /// …and round a wall, without walking through it. The path is the easy part;
    /// staying on the mesh while being steered is the part that goes wrong.
    #[test]
    fn it_walks_round_a_wall_rather_than_through_it() {
        // Two rooms joined by a doorway at the far end.
        let mut tris = slab(0.0, 0.0, 5.0, 8.0, 0.0);
        tris.extend(slab(5.0, 6.0, 2.0, 2.0, 0.0));
        tris.extend(slab(7.0, 0.0, 5.0, 8.0, 0.0));
        let mesh = bake(&tris, &settings()).unwrap();
        let mut crowd = Crowd::default();
        let id = crowd.add(AgentParams::default(), [2.0, 0.0, 2.0]);
        crowd.agent_mut(id).unwrap().move_to([10.0, 0.0, 2.0]);

        let dt = 1.0 / 60.0;
        let mut through_the_wall = false;
        for _ in 0..1500 {
            crowd.step(Some(&mesh), dt);
            let p = crowd.agent(id).unwrap().pos;
            // The wall is the strip between x=5 and x=7 below z=6.
            if p[0] > 5.05 && p[0] < 6.95 && p[2] < 5.9 {
                through_the_wall = true;
            }
        }
        assert!(!through_the_wall, "the agent cut the corner through solid wall");
        let a = crowd.agent(id).unwrap();
        assert_eq!(a.state(), AgentState::Arrived, "ended at {:?}", a.pos);
    }

    /// A goal on another island is not a reason to stand still, and it is not a
    /// reason to keep pushing at a wall forever either.
    #[test]
    fn an_unreachable_order_ends_as_blocked_rather_than_as_silence() {
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(20.0, 0.0, 4.0, 4.0, 0.0));
        let mesh = bake(&tris, &settings()).unwrap();
        let mut crowd = Crowd::default();
        let id = crowd.add(AgentParams::default(), [1.5, 0.0, 2.0]);
        crowd.agent_mut(id).unwrap().move_to([22.0, 0.0, 2.0]);

        run(&mut crowd, &mesh, 15.0);
        let a = crowd.agent(id).unwrap();
        assert!(!a.route_complete(), "the far island is not reachable");
        assert_eq!(a.state(), AgentState::Blocked, "at {:?}", a.pos);
        assert!(a.pos[0] > 2.5, "it should have walked to the near edge first: {:?}", a.pos);
    }

    /// Two dozen units sent to one spot must end up standing around it, not
    /// inside each other.
    #[test]
    fn a_crowd_ordered_onto_one_point_does_not_become_one_unit() {
        let mesh = bake(&slab(0.0, 0.0, 20.0, 20.0, 0.0), &settings()).unwrap();
        let mut crowd = Crowd { paths_per_step: 32, ..Default::default() };
        let mut ids = Vec::new();
        for i in 0..24 {
            let (x, z) = ((i % 6) as f32 * 0.9 + 2.0, (i / 6) as f32 * 0.9 + 2.0);
            let id = crowd.add(AgentParams { radius: 0.4, ..Default::default() }, [x, 0.0, z]);
            crowd.agent_mut(id).unwrap().move_to([15.0, 0.0, 15.0]);
            ids.push(id);
        }
        // Long enough for the whole ring to settle — the outermost units only
        // count as arrived once they touch somebody who already has.
        run(&mut crowd, &mesh, 30.0);

        let mut worst = f32::INFINITY;
        for (i, a) in ids.iter().enumerate() {
            for b in ids.iter().skip(i + 1) {
                let (pa, pb) = (crowd.agent(*a).unwrap().pos, crowd.agent(*b).unwrap().pos);
                worst = worst.min(flat_dist(pa, pb));
            }
        }
        // Not the full 0.8 they would like — a crowd presses together — but
        // nothing like standing on the same spot.
        assert!(worst > 0.35, "two units ended up {worst:.2} apart, which is inside each other");

        // ALL of them: the point itself only holds one, and arrival being
        // contagious (touching an arrived friend with the same order counts)
        // is what lets the rest settle instead of grinding forever.
        let arrived = ids.iter().filter(|id| crowd.agent(**id).unwrap().arrived()).count();
        assert_eq!(arrived, 24, "only {arrived} of 24 settled");
    }

    /// The frame or two between an order and the search that answers it must
    /// read as Moving — scripts check state the same frame they give orders,
    /// and an empty path is not an arrival.
    #[test]
    fn a_fresh_order_is_not_arrived_before_its_search_runs() {
        let mesh = bake(&slab(0.0, 0.0, 12.0, 12.0, 0.0), &settings()).unwrap();
        // Starve the budget: the order is given and never served.
        let mut crowd = Crowd { paths_per_step: 0, ..Default::default() };
        let id = crowd.add(AgentParams::default(), [1.5, 0.0, 1.5]);
        crowd.agent_mut(id).unwrap().move_to([10.0, 0.0, 10.0]);
        for _ in 0..30 {
            crowd.step(Some(&mesh), 1.0 / 60.0);
        }
        let a = crowd.agent(id).unwrap();
        assert_eq!(a.state(), AgentState::Moving, "no search has answered yet");
        assert!(
            flat_dist(a.pos, [1.5, 0.0, 1.5]) < 0.01,
            "it holds position rather than guessing: {:?}",
            a.pos
        );
    }

    /// A hundred orders at once must not run a hundred searches in one step.
    #[test]
    fn searches_are_spread_over_frames_rather_than_spiking_one() {
        let mesh = bake(&slab(0.0, 0.0, 30.0, 30.0, 0.0), &settings()).unwrap();
        let mut crowd = Crowd { paths_per_step: 4, ..Default::default() };
        let ids: Vec<AgentId> = (0..40)
            .map(|i| {
                let id = crowd.add(AgentParams::default(), [2.0 + (i % 10) as f32, 0.0, 2.0]);
                crowd.agent_mut(id).unwrap().move_to([25.0, 0.0, 25.0]);
                id
            })
            .collect();

        crowd.step(Some(&mesh), 1.0 / 60.0);
        let with_path = ids.iter().filter(|id| crowd.agent(**id).unwrap().has_path()).count();
        assert!(with_path <= 4, "{with_path} searches ran in one step, and the budget was 4");

        // …and everybody is served within a few steps, rather than starved.
        for _ in 0..12 {
            crowd.step(Some(&mesh), 1.0 / 60.0);
        }
        let with_path = ids.iter().filter(|id| crowd.agent(**id).unwrap().has_path()).count();
        assert_eq!(with_path, ids.len(), "some agent never got its turn");
    }

    /// A link is crossed as a link — reported the whole way, so a script can
    /// play the climb.
    #[test]
    fn a_link_is_crossed_and_says_so_while_it_happens() {
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(9.0, 0.0, 4.0, 4.0, 0.0));
        let mut ladder = OffLink::new(7, "ladder", [3.5, 0.0, 2.0], [9.5, 0.0, 2.0]);
        ladder.bidirectional = true;
        let mesh = bake_with(&tris, &settings(), &[], vec![ladder]).unwrap();
        assert!(mesh.off_links[0].resolved(), "both ends of the ladder must find ground");

        let mut crowd = Crowd::default();
        let id = crowd.add(AgentParams::default(), [1.0, 0.0, 2.0]);
        crowd.agent_mut(id).unwrap().move_to([11.5, 0.0, 2.0]);

        let dt = 1.0 / 60.0;
        let mut saw_crossing = false;
        for _ in 0..1200 {
            crowd.step(Some(&mesh), dt);
            let a = crowd.agent(id).unwrap();
            if a.state() == AgentState::Crossing {
                saw_crossing = true;
                let ride = a.crossing().expect("crossing without a ride");
                assert_eq!(ride.link, 7);
                assert!(ride.forwards);
                assert!((0.0..=1.0).contains(&ride.progress));
            }
        }
        assert!(saw_crossing, "the agent never reported being on the link");
        let a = crowd.agent(id).unwrap();
        assert_eq!(a.state(), AgentState::Arrived, "ended at {:?}", a.pos);
        assert!(a.pos[0] > 9.0, "it should be on the far island: {:?}", a.pos);
    }

    /// Shutting a door has to reach the units already walking to it.
    #[test]
    fn closing_a_link_makes_everyone_using_it_think_again() {
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(9.0, 0.0, 4.0, 4.0, 0.0));
        let door = OffLink::new(3, "door", [3.5, 0.0, 2.0], [9.5, 0.0, 2.0]);
        let mut mesh = bake_with(&tris, &settings(), &[], vec![door]).unwrap();

        let mut crowd = Crowd::default();
        let id = crowd.add(AgentParams::default(), [1.0, 0.0, 2.0]);
        crowd.agent_mut(id).unwrap().move_to([11.5, 0.0, 2.0]);
        run(&mut crowd, &mesh, 0.15);
        assert!(crowd.agent(id).unwrap().route_complete(), "the door is open");

        assert!(mesh.set_link_enabled(3, false));
        crowd.navmesh_changed();
        run(&mut crowd, &mesh, 8.0);
        let a = crowd.agent(id).unwrap();
        assert!(!a.route_complete(), "with the door shut there is no way over");
        assert!(a.pos[0] < 4.5, "and it must not have crossed anyway: {:?}", a.pos);
    }

    /// …but somebody already on it finishes the crossing.
    ///
    /// Stopping dead halfway through a door — or halfway up a ladder — leaves a
    /// character somewhere the navmesh does not cover, which is the one state
    /// nothing downstream can recover from.
    #[test]
    fn a_link_that_shuts_mid_crossing_still_lets_you_off_the_other_end() {
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(9.0, 0.0, 4.0, 4.0, 0.0));
        let door = OffLink::new(3, "door", [3.5, 0.0, 2.0], [9.5, 0.0, 2.0]);
        let mut mesh = bake_with(&tris, &settings(), &[], vec![door]).unwrap();

        let mut crowd = Crowd::default();
        let id = crowd.add(AgentParams::default(), [1.0, 0.0, 2.0]);
        crowd.agent_mut(id).unwrap().move_to([11.5, 0.0, 2.0]);
        // Long enough to be on it.
        run(&mut crowd, &mesh, 1.5);
        assert_eq!(crowd.agent(id).unwrap().state(), AgentState::Crossing);

        assert!(mesh.set_link_enabled(3, false));
        crowd.navmesh_changed();
        run(&mut crowd, &mesh, 10.0);
        let a = crowd.agent(id).unwrap();
        assert!(a.pos[0] > 9.0, "it should have finished the crossing: {:?}", a.pos);
        assert_eq!(a.state(), AgentState::Arrived);
    }

    /// A handle to a removed agent must not quietly become a handle to whoever
    /// takes the slot next.
    #[test]
    fn a_stale_handle_stays_stale() {
        let mut crowd = Crowd::default();
        let first = crowd.add(AgentParams::default(), [0.0; 3]);
        crowd.remove(first);
        let second = crowd.add(AgentParams::default(), [5.0, 0.0, 5.0]);
        assert!(crowd.agent(first).is_none(), "the old handle came back to life");
        assert!(crowd.agent(second).is_some());
        assert_ne!(first, second);
    }

    /// An order given before there is a navmesh is remembered, not dropped —
    /// scripts start before a bake is loaded more often than anyone expects.
    #[test]
    fn an_order_given_with_no_navmesh_survives_until_there_is_one() {
        let mut crowd = Crowd::default();
        let id = crowd.add(AgentParams::default(), [1.5, 0.0, 1.5]);
        crowd.agent_mut(id).unwrap().move_to([8.0, 0.0, 8.0]);
        for _ in 0..60 {
            crowd.step(None, 1.0 / 60.0);
        }
        assert_eq!(crowd.agent(id).unwrap().pos, [1.5, 0.0, 1.5], "nothing to walk on yet");

        let mesh = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &settings()).unwrap();
        run(&mut crowd, &mesh, 12.0);
        assert!(crowd.agent(id).unwrap().arrived(), "the order was still standing");
    }

    /// Two agents walking into each other must both get somewhere.
    #[test]
    fn two_agents_meeting_head_on_get_past_each_other() {
        let mesh = bake(&slab(0.0, 0.0, 16.0, 16.0, 0.0), &settings()).unwrap();
        let mut crowd = Crowd::default();
        let a = crowd.add(AgentParams::default(), [2.0, 0.0, 8.0]);
        let b = crowd.add(AgentParams::default(), [14.0, 0.0, 8.0]);
        crowd.agent_mut(a).unwrap().move_to([14.0, 0.0, 8.0]);
        crowd.agent_mut(b).unwrap().move_to([2.0, 0.0, 8.0]);
        run(&mut crowd, &mesh, 25.0);

        assert!(crowd.agent(a).unwrap().pos[0] > 11.0, "{:?}", crowd.agent(a).unwrap().pos);
        assert!(crowd.agent(b).unwrap().pos[0] < 5.0, "{:?}", crowd.agent(b).unwrap().pos);
    }
}
