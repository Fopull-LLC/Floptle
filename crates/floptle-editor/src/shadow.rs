//! A **second, headless simulation** of a rollback match, driven from an input
//! log (`docs/rollback-netcode-design.md` §5, §7 P6).
//!
//! Two features want this, and they want the same thing, which is why they are
//! built together rather than one growing a private copy the other would have
//! to refactor:
//!
//! - **The referee.** The host runs the match a second time at the *confirmed
//!   frontier only* — never guessing, never rolling back — and holds the
//!   authoritative result. A player's own copy is always a few ticks out on
//!   speculation; this one is never wrong, only late. That is the anti-cheat
//!   story, and it costs one more sim instance and no new netcode at all.
//! - **Match replays.** Inputs plus the seed *are* the replay file, so playback
//!   is not playback: it is running the match again in a fresh world. Which
//!   means a replay can be stepped, watched from a different camera, or diffed
//!   against another run of the same log.
//!
//! Both are the same object with a different stopping rule, so [`ShadowSim`]
//! takes the rule as a parameter and nothing else differs.
//!
//! ## Why it can be trusted
//!
//! Because it is not a re-implementation. It builds a `World`, a `Sim` and a
//! `ScriptHost` exactly as Play does, and advances them with the same
//! [`RollbackDriver`] the live session uses — the one that runs `simulate_tick`
//! for live and replayed ticks alike. A second code path would agree with the
//! first by coincidence; this agrees by construction, and the round-trip test
//! (`a_recorded_match_replays_to_the_same_state`) is what holds it to that.

use std::path::{Path, PathBuf};

use floptle_core::World;
use floptle_net::{InputLog, PeerId};
use floptle_physics::Sim;
use floptle_script::ScriptHost;

use crate::rollback::{Ctx, RollbackDriver};

/// How far a shadow simulation is allowed to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Horizon {
    /// Only ticks every peer's input has actually arrived for. The referee's
    /// rule: it is never wrong, only behind.
    Confirmed,
    /// Everything in the log. A replay's rule — the match is over, every input
    /// is in, and there is nothing left to guess about.
    WholeLog,
}

/// A headless second simulation of one match.
pub struct ShadowSim {
    pub world: World,
    pub sim: Sim,
    pub host: ScriptHost,
    pub driver: RollbackDriver,
    pub log: InputLog,
    step: f32,
    /// The newest tick actually simulated here.
    at: u64,
    /// Entries already handed to the driver. A `(peer, tick)` set rather than a
    /// high-water mark: a late input lands BELOW the frontier by definition —
    /// that is what makes it late — and a watermark would step straight over
    /// the one case the referee exists to handle.
    fed: std::collections::HashSet<(PeerId, u64)>,
}

impl ShadowSim {
    /// Build a shadow of `log`'s match from a scene document.
    ///
    /// `local` is which peer this instance stands in for. For the referee that
    /// is the host; for a replay it is arbitrary — every peer's input is in the
    /// log, so nothing is ever sampled and the choice cannot affect the result.
    pub fn build(
        doc: &floptle_scene::SceneDoc,
        script_dir: &Path,
        input_map: floptle_input::InputMap,
        log: InputLog,
        local: PeerId,
        step: f32,
    ) -> Self {
        let mut world = World::default();
        floptle_scene::spawn_into(doc, &mut world);
        let mut sim = Sim::build(
            &world,
            &[],
            floptle_physics::GravityField::uniform(floptle_core::math::Vec3::ZERO),
            floptle_core::math::DVec3::ZERO,
        );
        let mut host = ScriptHost::new();
        host.set_input_map(input_map);
        // One pass to build the script instances, exactly as Play does. The
        // driver's own passes fire `start` from here on.
        host.run(&mut world, script_dir, step, 0.0);

        let mut driver =
            RollbackDriver::new(local, log.peers.clone(), log.input_delay, log.seed);
        driver.rebind(&world, &mut sim, &host);
        Self { world, sim, host, driver, log, step, at: 0, fed: Default::default() }
    }

    /// The newest tick this shadow has simulated.
    pub fn tick(&self) -> u64 {
        self.at
    }

    /// The horizon this shadow may currently reach.
    pub fn horizon(&self, rule: Horizon) -> u64 {
        match rule {
            Horizon::WholeLog => self.log.last_tick(),
            // The newest tick every peer has an input for. Walked from the
            // current position rather than from 0 so a long match doesn't
            // re-scan its whole history every frame.
            Horizon::Confirmed => {
                let mut t = self.at;
                while t < self.log.last_tick() && self.tick_is_complete(t + 1) {
                    t += 1;
                }
                t
            }
        }
    }

    fn tick_is_complete(&self, tick: u64) -> bool {
        self.log.peers.iter().all(|p| self.log.at(tick).any(|e| e.peer == *p))
    }

    /// Simulate forward to the horizon, at most `max_ticks` of it — a referee
    /// catching up after a hitch must not stall the frame it is catching up in.
    /// Returns how many ticks were simulated.
    pub fn advance(&mut self, rule: Horizon, max_ticks: u64) -> u64 {
        let target = self.horizon(rule).min(self.at + max_ticks);
        let mut done = 0;
        while self.at < target {
            let next = self.at + 1;
            // Feed every input for ticks up to and including the delay window
            // ahead, so the driver holds them before it needs them.
            self.feed_through(next + self.log.input_delay as u64);
            let mut ctx = Ctx {
                world: &mut self.world,
                sim: &mut self.sim,
                host: &mut self.host,
                step: self.step,
            };
            if self.driver.advance(&mut ctx).is_none() {
                break; // stalled: the log doesn't reach here yet
            }
            self.at = next;
            done += 1;
        }
        done
    }

    /// Hand the driver every logged input at or below `tick` it hasn't seen.
    ///
    /// Fed for EVERY peer including the one this instance stands in for. In a
    /// live session the local peer's input is sampled and `add_local` shifts it
    /// by the delay; here it is already an applied tick straight out of the log,
    /// so shifting it again would replay the match a couple of ticks skewed.
    fn feed_through(&mut self, tick: u64) {
        let fresh: Vec<(PeerId, u64, floptle_net::NetInput)> = self
            .log
            .entries
            .iter()
            .filter(|e| e.tick <= tick && !self.fed.contains(&(e.peer, e.tick)))
            .map(|e| (e.peer, e.tick, e.input.clone()))
            .collect();
        for (peer, t, input) in fresh {
            self.fed.insert((peer, t));
            self.driver.feed_logged(peer, t, input);
        }
    }

    /// The state checksum for a simulated tick, or `None` once it has fallen
    /// out of the driver's ring.
    pub fn state_hash(&self, tick: u64) -> Option<u64> {
        self.driver.state_hash(tick)
    }
}

/// Where a project keeps its replays.
pub fn replay_dir(project_root: &Path) -> PathBuf {
    project_root.join("replays")
}

/// Recorded matches in this project, newest name first.
///
/// A free function rather than an `Editor` method because the panel builds its
/// list mid-render, with half the editor already mutably borrowed for the GPU.
pub fn list_replays(project_root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(replay_dir(project_root)) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "floptlereplay"))
        .filter_map(|p| p.file_stem().map(|s| (s.to_string_lossy().to_string(), p.clone())))
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(12);
    out
}

/// Does this scene simulate anything by rollback? A shadow of a scene with no
/// fighters would run forever and prove nothing.
pub fn scene_has_rollback(doc: &floptle_scene::SceneDoc) -> bool {
    doc.nodes.iter().any(|n| n.net.as_ref().is_some_and(|r| r.rollback))
}

#[cfg(test)]
mod tests {
    use floptle_core::math::DVec3;
    use floptle_core::{Name, Replicated, ReplicationMode, RigidBody, ScriptInst, Scripts};
    use floptle_core::transform::Transform;
    use floptle_input::{Action, Binding, InputMap, Key, Source};
    use floptle_net::{NetInput, SERVER};

    use super::*;

    const STEP: f32 = 1.0 / 60.0;

    /// The same determinism profile the design documents: integer counters, no
    /// wall clock, no unseeded rng, everything the sim reads inside snapshot().
    const FIGHTER: &str = "\
state = { frame = 0, hp = 100, vx = 0, atk = 0, hits = 0 }\n\
function fixedUpdate(node, dt)\n\
  local pad = input.player(params.player)\n\
  state.frame = state.frame + 1\n\
  local dir = 0\n\
  if pad.action(\"Right\") then dir = dir + 1 end\n\
  if pad.action(\"Left\") then dir = dir - 1 end\n\
  state.vx = dir * 3.0\n\
  node.vx = state.vx\n\
  if state.atk > 0 then state.atk = state.atk - 1 end\n\
  if pad.justPressed(\"Punch\") and state.atk == 0 then\n\
    state.atk = 6\n\
    state.hits = state.hits + 1\n\
    -- A draw from the seeded stream: a replay that re-seeds per tick but not\n\
    -- per DRAW reproduces the match right up until two calls land in one tick.\n\
    state.hp = state.hp - net.random(1, 5)\n\
  end\n\
end\n\
function snapshot()\n\
  local c = {}\n\
  for k, v in pairs(state) do c[k] = v end\n\
  return c\n\
end\n\
function restore(s)\n\
  for k in pairs(state) do state[k] = nil end\n\
  for k, v in pairs(s) do state[k] = v end\n\
end\n";

    fn fighter_map() -> InputMap {
        InputMap {
            actions: vec![
                Action { name: "Left".into(), bindings: vec![Binding::new(Source::Key(Key::KeyA))] },
                Action { name: "Right".into(), bindings: vec![Binding::new(Source::Key(Key::KeyD))] },
                Action { name: "Punch".into(), bindings: vec![Binding::new(Source::Key(Key::KeyJ))] },
            ],
            axes1: vec![],
            axes2: vec![],
            motions: vec![],
            players: 2,
            motion_axis: None,
        }
    }

    const LEFT: u64 = 1 << 0;
    const RIGHT: u64 = 1 << 1;
    const PUNCH: u64 = 1 << 2;

    fn held(a: u64) -> NetInput {
        NetInput { actions: a, ..Default::default() }
    }
    fn press(a: u64) -> NetInput {
        NetInput { actions: a, just_pressed: a, ..Default::default() }
    }

    fn script_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("floptle_shadow_{tag}"));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("fighter.lua"), FIGHTER).unwrap();
        dir
    }

    /// A two-fighter scene, as a document — the thing a replay is played from.
    fn ring() -> floptle_scene::SceneDoc {
        let mut w = World::default();
        for i in 0..2 {
            let e = w.spawn();
            w.insert(e, Name(format!("P{}", i + 1)));
            w.insert(e, Transform::from_translation(DVec3::new(i as f64 * 4.0 - 2.0, 1.0, 0.0)));
            w.insert(e, RigidBody { radius: 0.5, gravity: false, ..Default::default() });
            w.insert(e, Replicated { mode: ReplicationMode::Rollback, ..Default::default() });
            w.insert(
                e,
                Scripts(vec![ScriptInst {
                    kind: "fighter".into(),
                    enabled: true,
                    params: vec![("player".into(), i as f32 + 1.0)],
                    refs: Vec::new(),
                    strs: Vec::new(),
                }]),
            );
        }
        floptle_scene::to_doc("ring", &w)
    }

    /// A scripted match, as an already-recorded log.
    fn recorded_match(peers: Vec<u64>) -> InputLog {
        let mut log = InputLog::new("ring", 0xD1CE_5EED, 2, peers.clone(), 0);
        for t in 1..=30u64 {
            let p1 = match t % 7 {
                0 => press(PUNCH),
                1 | 2 => held(RIGHT),
                3 => held(RIGHT | PUNCH),
                4 => held(LEFT),
                _ => held(0),
            };
            let p2 = match t % 5 {
                0 => press(PUNCH),
                1 => held(LEFT),
                2 | 3 => held(RIGHT),
                _ => held(0),
            };
            log.record(peers[0], t, &p1);
            log.record(peers[1], t, &p2);
        }
        log
    }

    /// Everything a divergence would show up in.
    fn fingerprint(s: &mut ShadowSim) -> String {
        let mut out = String::new();
        for n in s.driver.nodes() {
            let b = s.sim.body_snapshot(n.eid).expect("body");
            out.push_str(&format!(
                "#{} p={:016x},{:016x} v={:08x} ",
                n.eid,
                b.pos.x.to_bits(),
                b.pos.y.to_bits(),
                b.vel.x.to_bits(),
            ));
            let st = s.host.snapshot_scripts(n.eid);
            let mut kv: Vec<String> = st
                .entries
                .iter()
                .map(|(k, v)| format!("{k}={:016x}", v.canonical_hash()))
                .collect();
            kv.sort();
            out.push_str(&kv.join(","));
            out.push('\n');
        }
        out
    }

    /// THE property both features rest on: the inputs and the seed are the
    /// match. Two fresh worlds fed the same log must end up in bit-identical
    /// states — otherwise a replay is a re-enactment, and a referee's verdict
    /// is just a second opinion.
    #[test]
    fn a_recorded_match_replays_to_the_same_state() {
        let (doc, dir) = (ring(), script_dir("replay"));
        let log = recorded_match(vec![SERVER, 1]);

        let mut a = ShadowSim::build(&doc, &dir, fighter_map(), log.clone(), SERVER, STEP);
        a.advance(Horizon::WholeLog, 10_000);
        let mut b = ShadowSim::build(&doc, &dir, fighter_map(), log.clone(), SERVER, STEP);
        b.advance(Horizon::WholeLog, 10_000);

        assert_eq!(a.tick(), 30, "the replay must reach the end of the log");
        assert_eq!(a.tick(), b.tick());
        assert_eq!(fingerprint(&mut a), fingerprint(&mut b), "a replay is the match, run again");
    }

    /// A replay taken from the OTHER peer's seat is the same match. Nothing in
    /// a shadow is ever sampled, so whose seat it sits in cannot matter — and
    /// if it ever did, replays would disagree with the referee.
    #[test]
    fn which_peer_a_shadow_stands_in_for_does_not_change_the_match() {
        let (doc, dir) = (ring(), script_dir("seat"));
        let log = recorded_match(vec![SERVER, 1]);
        let mut host = ShadowSim::build(&doc, &dir, fighter_map(), log.clone(), SERVER, STEP);
        let mut joiner = ShadowSim::build(&doc, &dir, fighter_map(), log, 1, STEP);
        host.advance(Horizon::WholeLog, 10_000);
        joiner.advance(Horizon::WholeLog, 10_000);
        assert_eq!(fingerprint(&mut host), fingerprint(&mut joiner));
    }

    /// The referee's whole discipline in one test: it simulates a tick only
    /// once it holds every peer's input for it. Never guessing is what makes
    /// its result authoritative rather than merely early.
    #[test]
    fn the_referee_stops_at_the_confirmed_frontier_and_never_guesses() {
        let (doc, dir) = (ring(), script_dir("referee"));
        // A log missing peer 1's input for tick 12 — a packet still in the air.
        let mut log = recorded_match(vec![SERVER, 1]);
        log.entries.retain(|e| !(e.tick == 12 && e.peer == 1));

        let mut r = ShadowSim::build(&doc, &dir, fighter_map(), log.clone(), SERVER, STEP);
        r.advance(Horizon::Confirmed, 10_000);
        assert_eq!(r.tick(), 11, "tick 12 is not confirmed, so the referee has not run it");

        // The same log, played as a REPLAY, runs past it — a replay is allowed
        // to guess, because by then the match is over and nothing it decides
        // can contradict a peer.
        let mut p = ShadowSim::build(&doc, &dir, fighter_map(), log, SERVER, STEP);
        p.advance(Horizon::WholeLog, 10_000);
        assert!(p.tick() > 11, "a replay is not bound by the confirmed frontier");
    }

    /// The missing input turns up; the referee carries on from where it was
    /// rather than restarting, and lands on the same state as an uninterrupted
    /// run of the completed log.
    #[test]
    fn the_referee_catches_up_when_the_late_input_arrives() {
        let (doc, dir) = (ring(), script_dir("catchup"));
        let full = recorded_match(vec![SERVER, 1]);
        let mut partial = full.clone();
        let late = partial
            .entries
            .iter()
            .position(|e| e.tick == 12 && e.peer == 1)
            .map(|i| partial.entries.remove(i))
            .expect("the input to withhold");

        let mut r = ShadowSim::build(&doc, &dir, fighter_map(), partial, SERVER, STEP);
        r.advance(Horizon::Confirmed, 10_000);
        assert_eq!(r.tick(), 11);
        r.log.record(late.peer, late.tick, &late.input);
        r.advance(Horizon::Confirmed, 10_000);
        assert_eq!(r.tick(), 30, "with the gap filled it runs to the end");

        let mut straight = ShadowSim::build(&doc, &dir, fighter_map(), full, SERVER, STEP);
        straight.advance(Horizon::Confirmed, 10_000);
        assert_eq!(
            fingerprint(&mut r),
            fingerprint(&mut straight),
            "waiting for an input must cost time, never correctness"
        );
    }

    /// A referee must not stall the frame it is catching up in.
    #[test]
    fn catching_up_is_capped_per_call() {
        let (doc, dir) = (ring(), script_dir("cap"));
        let log = recorded_match(vec![SERVER, 1]);
        let mut r = ShadowSim::build(&doc, &dir, fighter_map(), log, SERVER, STEP);
        assert_eq!(r.advance(Horizon::Confirmed, 4), 4);
        assert_eq!(r.tick(), 4);
        r.advance(Horizon::Confirmed, 10_000);
        assert_eq!(r.tick(), 30);
    }
}
