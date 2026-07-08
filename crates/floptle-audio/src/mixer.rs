//! The mixer: named tracks with gain/pan/mute/solo and an effect chain, each
//! routing into another track (default: Master). Every playing voice mixes
//! into some track's input buffer; `MixerDsp::process` then folds the whole
//! graph down to the stereo output.
//!
//! [`MixerDesc`] is the serializable half (lives in `project.ron`, edited by
//! the Mixer tab, scriptable from Lua). [`MixerDsp`] is the audio-thread half;
//! `apply` diffs a new desc onto the running graph without dropping effect
//! tails or clicking.

use serde::{Deserialize, Serialize};

use crate::effects::{db_to_lin, EffectDesc, EffectDsp};
use crate::spatial::pan_gains;

/// Name of the always-present output track.
pub const MASTER: &str = "Master";

/// One effect in a track's chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectSlot {
    pub effect: EffectDesc,
    #[serde(default)]
    pub bypass: bool,
}

/// One mixer track's persisted state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackDesc {
    pub name: String,
    /// Fader gain in dB (0 = unity).
    #[serde(default)]
    pub gain_db: f32,
    /// Stereo pan -1..1.
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub soloed: bool,
    #[serde(default)]
    pub effects: Vec<EffectSlot>,
    /// Name of the track this one outputs into; `None` = Master.
    #[serde(default)]
    pub output: Option<String>,
}

impl TrackDesc {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
            soloed: false,
            effects: Vec::new(),
            output: None,
        }
    }
}

/// The whole mixer graph. Master is stored separately so it can't be deleted
/// or re-routed; user tracks are ordered as shown in the Mixer tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MixerDesc {
    pub master: TrackDesc,
    #[serde(default)]
    pub tracks: Vec<TrackDesc>,
}

impl Default for MixerDesc {
    fn default() -> Self {
        Self { master: TrackDesc::new(MASTER), tracks: Vec::new() }
    }
}

impl MixerDesc {
    /// Find a user track by name (case-sensitive; `Master` is not in here).
    pub fn track(&self, name: &str) -> Option<&TrackDesc> {
        self.tracks.iter().find(|t| t.name == name)
    }

    pub fn track_mut(&mut self, name: &str) -> Option<&mut TrackDesc> {
        self.tracks.iter_mut().find(|t| t.name == name)
    }

    /// A name not yet taken, for the mixer tab's "+ Track" button.
    pub fn fresh_name(&self, base: &str) -> String {
        if base != MASTER && self.track(base).is_none() {
            return base.to_string();
        }
        for i in 2.. {
            let name = format!("{base} {i}");
            if self.track(&name).is_none() {
                return name;
            }
        }
        unreachable!()
    }
}

/// Per-sample gain smoothing time constant — kills zipper noise on fader
/// drags without making the fader feel laggy.
const GAIN_SMOOTH_MS: f32 = 12.0;

struct TrackDsp {
    name: String,
    /// Chain with each slot's desc kept for diffing on `apply`.
    effects: Vec<(EffectSlot, Box<dyn EffectDsp>)>,
    /// Resolved output track index (into `MixerDsp::tracks`; 0 = master).
    out: usize,
    /// Smoothed post-fader gains per side.
    cur_l: f32,
    cur_r: f32,
    tgt_l: f32,
    tgt_r: f32,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    /// Post-fader peak of the last processed block (for UI meters).
    meter: f32,
}

impl TrackDsp {
    fn new(name: &str, block: usize) -> Self {
        Self {
            name: name.to_string(),
            effects: Vec::new(),
            out: 0,
            cur_l: 1.0,
            cur_r: 1.0,
            tgt_l: 1.0,
            tgt_r: 1.0,
            buf_l: vec![0.0; block],
            buf_r: vec![0.0; block],
            meter: 0.0,
        }
    }
}

/// The running mixer. Index 0 is always Master.
pub struct MixerDsp {
    tracks: Vec<TrackDsp>,
    /// Processing order for non-master tracks: deepest (furthest from master)
    /// first, so a track's output is complete before its destination runs.
    order: Vec<usize>,
    sample_rate: f32,
    block: usize,
    smooth: f32,
}

impl MixerDsp {
    pub fn new(sample_rate: f32, block: usize) -> Self {
        let mut m = Self {
            tracks: Vec::new(),
            order: Vec::new(),
            sample_rate,
            block,
            smooth: (-1.0 / (GAIN_SMOOTH_MS / 1000.0 * sample_rate)).exp(),
        };
        m.apply(&MixerDesc::default());
        m
    }

    /// Rebuild the graph from a desc, preserving DSP state wherever a track
    /// (by name) and an effect (by kind + position) survive the edit.
    pub fn apply(&mut self, desc: &MixerDesc) {
        let mut old: Vec<TrackDsp> = std::mem::take(&mut self.tracks);
        let all: Vec<&TrackDesc> =
            std::iter::once(&desc.master).chain(desc.tracks.iter()).collect();

        let mut next: Vec<TrackDsp> = Vec::with_capacity(all.len());
        let mut is_new: Vec<bool> = Vec::with_capacity(all.len());
        for (i, td) in all.iter().enumerate() {
            let name = if i == 0 { MASTER } else { td.name.as_str() };
            let mut dsp = match old.iter().position(|t| t.name == name) {
                Some(pos) => {
                    is_new.push(false);
                    old.swap_remove(pos)
                }
                None => {
                    is_new.push(true);
                    TrackDsp::new(name, self.block)
                }
            };
            // Effects: reuse the processor when the slot is the same kind.
            let mut old_fx = std::mem::take(&mut dsp.effects);
            dsp.effects = td
                .effects
                .iter()
                .enumerate()
                .map(|(slot, s)| {
                    let reuse = old_fx
                        .get_mut(slot)
                        .filter(|(prev, _)| prev.effect.same_kind(&s.effect))
                        .map(|(_, d)| std::mem::replace(d, noop_dsp()));
                    let mut d = reuse.unwrap_or_else(|| s.effect.build(self.sample_rate));
                    d.update(&s.effect);
                    (s.clone(), d)
                })
                .collect();
            next.push(dsp);
        }

        // Resolve routing: master (0) outputs nowhere; unknown names, self
        // routes, and cycles fall back to master.
        let name_of = |i: usize| if i == 0 { MASTER } else { all[i].name.as_str() };
        let mut outs = vec![0usize; next.len()];
        for i in 1..next.len() {
            outs[i] = all[i]
                .output
                .as_deref()
                .and_then(|o| (1..next.len()).find(|&j| j != i && name_of(j) == o))
                .unwrap_or(0);
        }
        // Break cycles: every chain must reach master within `len` hops.
        for i in 1..next.len() {
            let mut cur = i;
            let mut hops = 0;
            while cur != 0 && hops <= next.len() {
                cur = outs[cur];
                hops += 1;
            }
            if cur != 0 {
                outs[i] = 0;
            }
        }
        for (t, &o) in next.iter_mut().zip(outs.iter()) {
            t.out = o;
        }

        // Depth-sorted order (deepest first). Depth = hops to master.
        let depth = |i: usize| {
            let mut cur = i;
            let mut d = 0;
            while cur != 0 {
                cur = outs[cur];
                d += 1;
            }
            d
        };
        let mut order: Vec<usize> = (1..next.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(depth(i)));

        // Fader targets: mute/solo fold into the target gain so engage and
        // release both glide instead of clicking.
        let any_solo = all.iter().skip(1).any(|t| t.soloed);
        let mut audible = vec![!any_solo; next.len()];
        audible[0] = true;
        if any_solo {
            for (i, t) in all.iter().enumerate().skip(1) {
                if t.soloed {
                    // A soloed track and everything downstream of it stays on.
                    let mut cur = i;
                    while cur != 0 {
                        audible[cur] = true;
                        cur = outs[cur];
                    }
                }
            }
        }
        for (i, (t, td)) in next.iter_mut().zip(all.iter()).enumerate() {
            let on = !td.muted && audible[i];
            let g = if on { db_to_lin(td.gain_db) } else { 0.0 };
            let (pl, pr) = pan_gains(td.pan);
            t.tgt_l = g * pl;
            t.tgt_r = g * pr;
            if is_new[i] {
                // Brand-new track: snap to target instead of gliding in from
                // unity — a track born muted must be born silent.
                t.cur_l = t.tgt_l;
                t.cur_r = t.tgt_r;
            }
        }

        self.tracks = next;
        self.order = order;
    }

    /// Number of tracks including master.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Index a voice should mix into for a track name ("" or unknown → 0,
    /// Master).
    pub fn track_index(&self, name: &str) -> usize {
        if name.is_empty() {
            return 0;
        }
        self.tracks.iter().position(|t| t.name == name).unwrap_or(0)
    }

    /// The input buffers for a track — voices accumulate into these before
    /// `process` runs. `idx` out of range lands on master.
    pub fn input(&mut self, idx: usize) -> (&mut [f32], &mut [f32]) {
        let i = if idx < self.tracks.len() { idx } else { 0 };
        let t = &mut self.tracks[i];
        (&mut t.buf_l, &mut t.buf_r)
    }

    /// Fold the graph into the stereo output. `n` = frames this block
    /// (≤ block size). Track buffers are cleared for the next block.
    pub fn process(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        let n = out_l.len().min(self.block);
        for pass in 0..=self.order.len() {
            // Non-master tracks in depth order, then master (index 0).
            let i = if pass < self.order.len() { self.order[pass] } else { 0 };
            let t = &mut self.tracks[i];
            let (mut l_buf, mut r_buf) = (std::mem::take(&mut t.buf_l), std::mem::take(&mut t.buf_r));
            for (slot, dsp) in &mut t.effects {
                if !slot.bypass {
                    dsp.process(&mut l_buf[..n], &mut r_buf[..n]);
                }
            }
            let mut peak = 0.0f32;
            let (mut cl, mut cr) = (t.cur_l, t.cur_r);
            let (tl, tr) = (t.tgt_l, t.tgt_r);
            let smooth = self.smooth;
            for s in 0..n {
                cl = tl + smooth * (cl - tl);
                cr = tr + smooth * (cr - tr);
                l_buf[s] *= cl;
                r_buf[s] *= cr;
                peak = peak.max(l_buf[s].abs()).max(r_buf[s].abs());
            }
            let (out_idx, is_master) = (self.tracks[i].out, i == 0);
            {
                let t = &mut self.tracks[i];
                t.cur_l = cl;
                t.cur_r = cr;
                t.meter = peak;
            }
            if is_master {
                out_l[..n].copy_from_slice(&l_buf[..n]);
                out_r[..n].copy_from_slice(&r_buf[..n]);
            } else {
                let dst = &mut self.tracks[out_idx];
                for s in 0..n {
                    dst.buf_l[s] += l_buf[s];
                    dst.buf_r[s] += r_buf[s];
                }
            }
            // Hand the (cleared) buffers back for the next block.
            l_buf[..n].fill(0.0);
            r_buf[..n].fill(0.0);
            let t = &mut self.tracks[i];
            t.buf_l = l_buf;
            t.buf_r = r_buf;
        }
    }

    /// Post-fader peak level of every track from the last block, master
    /// first, for the mixer tab's meters.
    pub fn meters(&self, out: &mut Vec<(String, f32)>) {
        out.clear();
        for t in &self.tracks {
            out.push((t.name.clone(), t.meter));
        }
    }

    /// Clear all buffers and effect tails (Play stopped).
    pub fn reset(&mut self) {
        for t in &mut self.tracks {
            t.buf_l.fill(0.0);
            t.buf_r.fill(0.0);
            t.meter = 0.0;
            for (_, dsp) in &mut t.effects {
                dsp.reset();
            }
        }
    }
}

// A tiny placeholder so `apply` can move a boxed processor out of the old
// slot without leaving a hole. Never processes audio.
struct NoopDsp;
impl EffectDsp for NoopDsp {
    fn process(&mut self, _l: &mut [f32], _r: &mut [f32]) {}
    fn update(&mut self, _d: &EffectDesc) {}
    fn reset(&mut self) {}
}

fn noop_dsp() -> Box<dyn EffectDsp> {
    Box::new(NoopDsp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc_with(tracks: Vec<TrackDesc>) -> MixerDesc {
        MixerDesc { master: TrackDesc::new(MASTER), tracks }
    }

    #[test]
    fn routes_track_through_master() {
        let mut m = MixerDsp::new(48_000.0, 128);
        m.apply(&desc_with(vec![TrackDesc::new("SFX")]));
        let idx = m.track_index("SFX");
        assert_eq!(idx, 1);
        let (l, _r) = m.input(idx);
        l.fill(0.5);
        let mut out_l = vec![0.0; 128];
        let mut out_r = vec![0.0; 128];
        m.process(&mut out_l, &mut out_r);
        assert!(out_l[127] > 0.4, "signal did not reach master: {}", out_l[127]);
    }

    #[test]
    fn chained_routing_and_cycle_fallback() {
        let mut a = TrackDesc::new("A");
        a.output = Some("B".into());
        let mut b = TrackDesc::new("B");
        b.output = Some("A".into()); // cycle: both must fall back to master
        let mut m = MixerDsp::new(48_000.0, 64);
        m.apply(&desc_with(vec![a, b.clone()]));
        let (l, _) = m.input(1);
        l.fill(0.25);
        let mut out_l = vec![0.0; 64];
        let mut out_r = vec![0.0; 64];
        m.process(&mut out_l, &mut out_r);
        assert!(out_l[63] > 0.2, "cycled track was silently dropped");

        // Legit chain: A -> B -> master.
        let mut a2 = TrackDesc::new("A");
        a2.output = Some("B".into());
        b.output = None;
        m.apply(&desc_with(vec![a2, b]));
        let (l, _) = m.input(1);
        l.fill(0.25);
        m.process(&mut out_l, &mut out_r);
        assert!(out_l[63] > 0.2, "chained track lost signal");
    }

    #[test]
    fn mute_and_solo() {
        let mut sfx = TrackDesc::new("SFX");
        let music = TrackDesc::new("Music");
        sfx.muted = true;
        let mut m = MixerDsp::new(48_000.0, 256);
        m.apply(&desc_with(vec![sfx.clone(), music.clone()]));
        let (l, _) = m.input(1);
        l.fill(1.0);
        let mut out_l = vec![0.0; 256];
        let mut out_r = vec![0.0; 256];
        // Run a few blocks so smoothing settles.
        for _ in 0..8 {
            let (l, _) = m.input(1);
            l.fill(1.0);
            m.process(&mut out_l, &mut out_r);
        }
        assert!(out_l[255].abs() < 1e-3, "muted track leaked: {}", out_l[255]);

        // Solo Music: SFX (unmuted now) must go quiet. Run ~250 ms so the
        // 12 ms fader glide fully settles.
        sfx.muted = false;
        let mut music2 = music;
        music2.soloed = true;
        m.apply(&desc_with(vec![sfx, music2]));
        for _ in 0..48 {
            let (l, _) = m.input(1); // SFX
            l.fill(1.0);
            m.process(&mut out_l, &mut out_r);
        }
        assert!(out_l[255].abs() < 1e-3, "non-soloed track audible during solo");
    }

    #[test]
    fn desc_roundtrip() {
        let mut d = MixerDesc::default();
        let mut t = TrackDesc::new("Music");
        t.effects.push(EffectSlot {
            effect: EffectDesc::Reverb { room_size: 0.5, damping: 0.5, width: 1.0, mix: 0.3, pre_delay_ms: 10.0 },
            bypass: false,
        });
        t.output = Some("Bus".into());
        d.tracks.push(t);
        let s = ron::ser::to_string(&d).unwrap();
        let back: MixerDesc = ron::de::from_str(&s).unwrap();
        assert_eq!(d, back);
    }
}
