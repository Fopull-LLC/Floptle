//! A river as a ribbon: a line of points along its surface, each with how
//! wide and deep the water is there and how fast it runs.
//!
//! A game lays the points down the channel in its parent's frame. Between
//! them the centreline is a smooth curve through every point, and the
//! surface height, the half-width, the depth and the current are eased from
//! one point's value to the next without ever leaving the range between
//! them, so a river never runs uphill between two points or pinches to a
//! negative width. The curve is cut into short straight pieces once, when the
//! river is declared, and every question afterwards (how deep is this point,
//! which way does the water run, where are the surface's triangles) is asked
//! of those same pieces, so the river that floats a crate is the river that
//! is drawn.
//!
//! A point is in the river when the nearest place on the centreline is no
//! further off than the half-width there. Measuring to the nearest place,
//! rather than across each straight piece, is what makes a bend whole: the
//! outside of a turn has no wedge of dry ground between two pieces, and the
//! inside has no strip claimed twice.

use std::collections::HashMap;

use glam::DVec3;

/// The most points a ribbon is declared with.
pub const MAX_POINTS: usize = 4096;

/// The most straight pieces one stretch between two points is cut into.
const MAX_PIECES: usize = 64;

/// How far one piece may turn from the last, radians: about six degrees.
const TURN: f64 = 0.1;

/// How far under its bed a ribbon still answers as water, metres. The bed is
/// a shape the game described, not the ground itself, so a body resting on
/// the real bottom a little below it is still wet.
pub const BED_SLACK: f64 = 0.5;

/// One point a ribbon was declared with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RibbonPoint {
    /// On the water's surface, in the middle of the channel, in the frame of
    /// what the river lies on.
    pub pos: DVec3,
    /// From the middle to either bank, metres.
    pub half_width: f64,
    /// From the surface to the bed in the middle of the channel, metres;
    /// infinite for a river with no bed.
    pub depth: f64,
    /// How fast the water runs along the channel towards the next point,
    /// m/s; negative runs back towards the first.
    pub current: f64,
}

/// The shape of the channel's bed from bank to bank.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bed {
    /// A gentle U: the full depth in the middle, falling away to nothing at
    /// the banks as a parabola, so the water shallows towards the shore.
    #[default]
    Round,
    /// The same depth from bank to bank: a canal, a flume.
    Flat,
}

/// One end of a straight piece of the centreline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Station {
    /// On the surface, in the middle of the channel.
    pub pos: DVec3,
    /// Out of the water here.
    pub up: DVec3,
    /// Downstream along the surface, slope and all.
    pub tangent: DVec3,
    /// Across the channel, level, at right angles to the flow: the bank on
    /// the right of someone looking downstream with +Y up and −Z ahead.
    pub side: DVec3,
    /// From `pos` to the bank on the far side from `side` and to the bank
    /// on its side, level: `side` times the half-width either way, except
    /// on the inside of a straight river's corner, where the bank is the
    /// one point the two reaches' banks meet at.
    pub banks: [DVec3; 2],
    /// The surface's height: along +Y on a plane, from the frame's origin on
    /// a round world.
    pub level: f64,
    /// Metres down the channel from the first point.
    pub along: f64,
    pub half_width: f64,
    pub depth: f64,
    pub current: f64,
}

/// Where a point is on a ribbon.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Located {
    /// Metres down the channel from the first point.
    pub along: f64,
    /// Metres from the middle of the channel, 0 or more.
    pub across: f64,
    /// The half-width there.
    pub half_width: f64,
    /// The surface's height there, as [`Station::level`], and the point's own
    /// height measured the same way.
    pub level: f64,
    pub height: f64,
    /// How far below the surface the bed lies under this point; infinite for
    /// a river with no bed.
    pub bed: f64,
    /// The water's velocity there, in the ribbon's frame.
    pub flow: DVec3,
    /// Out of the water.
    pub up: DVec3,
}

/// A river's course: its points, the straight pieces its curve was cut into,
/// and an index over them.
#[derive(Clone, Debug)]
pub struct Ribbon {
    /// Up is the frame's +Y everywhere, and a height is a y; otherwise the
    /// river lies on a round world about the frame's origin, up is the
    /// radial and a height is a distance from the centre.
    pub flat: bool,
    pub bed: Bed,
    /// Straight lines from point to point rather than a curve through them.
    pub straight: bool,
    pub points: Vec<RibbonPoint>,
    pub stations: Vec<Station>,
    /// A number that is the same for the same course, for telling one
    /// surface from another without comparing every station.
    pub stamp: u64,
    index: Index,
}

/// Two courses are the same river when they were declared the same: the
/// pieces and the index follow from the points.
impl PartialEq for Ribbon {
    fn eq(&self, other: &Self) -> bool {
        self.stamp == other.stamp && self.flat == other.flat && self.bed == other.bed && self.straight == other.straight && self.points == other.points
    }
}

/// The straight pieces near each place, so a question about one point looks
/// at a handful of pieces however long the river is.
///
/// Pieces are filed by where they lie on the ground: the x–z plane for a
/// flat river, the sphere of the river's mean radius for one on a round
/// world. A point however high above the river or deep under it lands in the
/// cell of the ground under it.
#[derive(Clone, Debug)]
struct Index {
    cell: f64,
    /// The sphere pieces are filed on, for a river on a round world.
    radius: Option<f64>,
    lo: DVec3,
    hi: DVec3,
    /// Each cell's run of `pieces`.
    cells: HashMap<[i32; 3], (u32, u32)>,
    pieces: Vec<u32>,
}

impl Index {
    fn ground(&self, p: DVec3) -> Option<DVec3> {
        match self.radius {
            Some(r) => Some(p.try_normalize()? * r),
            None => Some(DVec3::new(p.x, 0.0, p.z)),
        }
    }

    fn key(&self, g: DVec3) -> [i32; 3] {
        [(g.x / self.cell).floor() as i32, (g.y / self.cell).floor() as i32, (g.z / self.cell).floor() as i32]
    }

    /// The pieces that could hold `p`.
    fn near(&self, p: DVec3) -> &[u32] {
        let Some(g) = self.ground(p) else { return &[] };
        if g.cmplt(self.lo).any() || g.cmpgt(self.hi).any() {
            return &[];
        }
        match self.cells.get(&self.key(g)) {
            Some(&(start, len)) => &self.pieces[start as usize..(start + len) as usize],
            None => &[],
        }
    }
}

/// A value eased through its points so it never overshoots them: a cubic
/// through each pair whose slopes are chosen (Fritsch and Carlson's rule) to
/// keep it between the two. `x` rises strictly.
struct Eased {
    x: Vec<f64>,
    y: Vec<f64>,
    slope: Vec<f64>,
}

impl Eased {
    fn new(x: &[f64], y: Vec<f64>) -> Self {
        let n = y.len();
        let mut slope = vec![0.0; n];
        if n >= 2 {
            let secant: Vec<f64> = (0..n - 1).map(|k| (y[k + 1] - y[k]) / (x[k + 1] - x[k])).collect();
            slope[0] = secant[0];
            slope[n - 1] = secant[n - 2];
            for k in 1..n - 1 {
                let (a, b) = (secant[k - 1], secant[k]);
                if a * b > 0.0 {
                    let (h0, h1) = (x[k] - x[k - 1], x[k + 1] - x[k]);
                    let (w0, w1) = (2.0 * h1 + h0, h1 + 2.0 * h0);
                    slope[k] = (w0 + w1) / (w0 / a + w1 / b);
                }
            }
        }
        Self { x: x.to_vec(), y, slope }
    }

    /// The value a fraction `u` of the way from point `k` to the next.
    fn at(&self, k: usize, u: f64) -> f64 {
        let h = self.x[k + 1] - self.x[k];
        let (u2, u3) = (u * u, u * u * u);
        (2.0 * u3 - 3.0 * u2 + 1.0) * self.y[k]
            + (u3 - 2.0 * u2 + u) * h * self.slope[k]
            + (-2.0 * u3 + 3.0 * u2) * self.y[k + 1]
            + (u3 - u2) * h * self.slope[k + 1]
    }
}

/// The curve through `p1` and `p2` a fraction `u` of the way between them,
/// shaped by the points either side: a centripetal Catmull–Rom spline, which
/// passes through every point and does not loop or overshoot where points
/// are unevenly spaced.
fn curve(p0: DVec3, p1: DVec3, p2: DVec3, p3: DVec3, u: f64) -> DVec3 {
    let knot = |a: DVec3, b: DVec3| (b - a).length().sqrt().max(1e-6);
    let t0 = 0.0;
    let t1 = t0 + knot(p0, p1);
    let t2 = t1 + knot(p1, p2);
    let t3 = t2 + knot(p2, p3);
    let t = t1 + (t2 - t1) * u;
    let a1 = p0 * ((t1 - t) / (t1 - t0)) + p1 * ((t - t0) / (t1 - t0));
    let a2 = p1 * ((t2 - t) / (t2 - t1)) + p2 * ((t - t1) / (t2 - t1));
    let a3 = p2 * ((t3 - t) / (t3 - t2)) + p3 * ((t - t2) / (t3 - t2));
    let b1 = a1 * ((t2 - t) / (t2 - t0)) + a2 * ((t - t0) / (t2 - t0));
    let b2 = a2 * ((t3 - t) / (t3 - t1)) + a3 * ((t - t1) / (t3 - t1));
    b1 * ((t2 - t) / (t2 - t1)) + b2 * ((t - t1) / (t2 - t1))
}

impl Ribbon {
    /// The river through `points` as a smooth curve, or why they do not
    /// make one.
    pub fn new(points: Vec<RibbonPoint>, flat: bool, bed: Bed) -> Result<Self, String> {
        Self::with(points, flat, bed, false)
    }

    /// The river through `points`. `straight` runs it in straight lines from
    /// each point to the next, every value running straight between them
    /// too, with the outside of each corner rounded; otherwise it is a
    /// smooth curve through the points with its values eased.
    pub fn with(points: Vec<RibbonPoint>, flat: bool, bed: Bed, straight: bool) -> Result<Self, String> {
        let n = points.len();
        if !(2..=MAX_POINTS).contains(&n) {
            return Err(format!("a ribbon is 2 to {MAX_POINTS} points, not {n}"));
        }
        let up_of = |p: DVec3| if flat { DVec3::Y } else { p.normalize_or_zero() };
        let level_of = |p: DVec3| if flat { p.y } else { p.length() };
        let no_bed = points.iter().any(|p| p.depth.is_infinite());
        for (i, p) in points.iter().enumerate() {
            if !p.pos.is_finite() || !p.current.is_finite() || p.depth.is_nan() {
                return Err(format!("point {} is not all numbers", i + 1));
            }
            if !(p.half_width.is_finite() && p.half_width > 0.0) {
                return Err(format!("point {}'s `halfWidth` is more than 0 metres", i + 1));
            }
            if p.depth <= 0.0 {
                return Err(format!("point {}'s `depth` is more than 0 metres", i + 1));
            }
            if !flat && p.pos.length() <= p.half_width {
                return Err(format!(
                    "point {} is at its parent's centre: a ribbon on a world lies round the world's centre, and `flat = true` lays it flat instead",
                    i + 1
                ));
            }
        }
        // How far each point is down the channel, by straight lines: what the
        // eased values are spaced by.
        let mut chord = vec![0.0; n];
        for i in 1..n {
            let step = points[i].pos - points[i - 1].pos;
            let up = up_of(points[i - 1].pos);
            let ground = (step - up * step.dot(up)).length();
            if ground < 1e-6 {
                return Err(format!("points {} and {} are at the same place or one above the other: a ribbon runs along the ground", i, i + 1));
            }
            chord[i] = chord[i - 1] + step.length();
        }
        let eased = |f: &dyn Fn(&RibbonPoint) -> f64| Eased::new(&chord, points.iter().map(f).collect());
        let level = eased(&|p| level_of(p.pos));
        let half_width = eased(&|p| p.half_width);
        let depth = eased(&|p| if no_bed { 0.0 } else { p.depth });
        let current = eased(&|p| p.current);
        // The curve runs on past each end as if the river carried straight on.
        let pos = |i: i64| -> DVec3 {
            if i < 0 {
                points[0].pos * 2.0 - points[1].pos
            } else if i as usize >= n {
                points[n - 1].pos * 2.0 - points[n - 2].pos
            } else {
                points[i as usize].pos
            }
        };
        let value = |e: &Eased, k: usize, u: f64| if straight { e.y[k] + (e.y[k + 1] - e.y[k]) * u } else { e.at(k, u) };
        let station = |k: usize, u: f64, on_curve: DVec3, tangent: DVec3| {
            let up = up_of(on_curve);
            let l = value(&level, k, u);
            Station {
                pos: if flat { DVec3::new(on_curve.x, l, on_curve.z) } else { up * l },
                up,
                tangent,
                side: DVec3::ZERO,
                banks: [DVec3::ZERO; 2],
                level: l,
                along: 0.0,
                half_width: value(&half_width, k, u).max(1e-6),
                depth: if no_bed { f64::INFINITY } else { value(&depth, k, u).max(0.0) },
                current: value(&current, k, u),
            }
        };
        // Where a straight river turns a corner: one at each point between
        // the first and the last where it turns more than a piece may.
        struct Corner {
            /// The headings into the corner and out of it, level.
            came: DVec3,
            goes: DVec3,
            /// Pieces in its fan.
            fan: usize,
            /// Which bank is the inside of the turn.
            inside: usize,
            /// From the point to where the inside banks of the two reaches
            /// meet, and how far back from the corner that is.
            meet: DVec3,
            back: f64,
        }
        let mut turns: Vec<Option<Corner>> = (0..n).map(|_| None).collect();
        for k in 1..n - 1 {
            let up = up_of(points[k].pos);
            let level = |v: DVec3| (v - up * v.dot(up)).normalize_or_zero();
            let (came, goes) = (level(points[k].pos - points[k - 1].pos), level(points[k + 1].pos - points[k].pos));
            let angle = came.dot(goes).clamp(-1.0, 1.0).acos();
            if !straight || angle <= TURN {
                continue;
            }
            // The inside banks meet on the line that halves the corner, no
            // further back than half the shorter reach.
            let width = points[k].half_width;
            let reach = (chord[k] - chord[k - 1]).min(chord[k + 1] - chord[k]);
            let back = (width * (angle / 2.0).tan()).min(reach / 2.0);
            turns[k] = Some(Corner {
                came,
                goes,
                fan: (angle / TURN).ceil() as usize,
                inside: usize::from(came.cross(goes).dot(up) < 0.0),
                meet: (goes - came).normalize_or_zero() * (width * width + back * back).sqrt(),
                back,
            });
        }
        let mut stations: Vec<Station> = Vec::new();
        // The stations of each corner's fan, which bank is the inside of
        // the turn, and where the banks of the two reaches meet there.
        let mut corners: Vec<(std::ops::Range<usize>, usize, DVec3)> = Vec::new();
        for k in 0..n - 1 {
            let at = |u: f64| {
                if straight {
                    points[k].pos + (points[k + 1].pos - points[k].pos) * u
                } else {
                    curve(pos(k as i64 - 1), pos(k as i64), pos(k as i64 + 1), pos(k as i64 + 2), u)
                }
            };
            // A straight river turns all at once at a point: the outside of
            // the corner is rounded by a fan of stations there, each turned
            // a little further.
            if let Some(c) = &turns[k] {
                corners.push((stations.len()..stations.len() + c.fan + 1, c.inside, c.meet));
                for j in 0..=c.fan {
                    let turned = (c.came + (c.goes - c.came) * (j as f64 / c.fan as f64)).try_normalize();
                    stations.push(station(k, 0.0, points[k].pos, turned.ok_or("a ribbon's points fold back on themselves")?));
                }
            } else {
                stations.push(station(k, 0.0, at(0.0), DVec3::ZERO));
            }
            // No station stands where a corner's inside banks have already
            // met: its own bank would lie past the meeting point.
            let reach = chord[k + 1] - chord[k];
            let clear = |u: f64| {
                turns[k].as_ref().is_none_or(|c| u * reach > c.back + 1e-9) && turns[k + 1].as_ref().is_none_or(|c| (1.0 - u) * reach > c.back + 1e-9)
            };
            // Walk the stretch finely and end a piece whenever it has turned
            // about six degrees or run about as far as the river is wide,
            // so a curve gets its stations where it bends.
            let longest = (2.0 * points[k].half_width.min(points[k + 1].half_width)).max(2.0);
            let (mut turned, mut run, mut before, mut from) = (0.0, 0.0, None::<DVec3>, at(0.0));
            for fine in 1..MAX_PIECES {
                let u = fine as f64 / MAX_PIECES as f64;
                let to = at(u);
                let step = to - from;
                let heading = step.normalize_or_zero();
                if let Some(b) = before {
                    turned += b.dot(heading).clamp(-1.0, 1.0).acos();
                }
                run += step.length();
                (before, from) = (Some(heading), to);
                if (turned >= TURN || run >= longest) && clear(u) {
                    stations.push(station(k, u, to, DVec3::ZERO));
                    (turned, run) = (0.0, 0.0);
                }
            }
            if k == n - 2 {
                stations.push(station(k, 1.0, at(1.0), DVec3::ZERO));
            }
        }
        for i in 0..stations.len() {
            // A corner's fan was given its headings; everywhere else a
            // station heads from the one before it to the one after.
            if stations[i].tangent == DVec3::ZERO {
                let (a, b) = (stations[i.saturating_sub(1)].pos, stations[(i + 1).min(stations.len() - 1)].pos);
                stations[i].tangent = (b - a).try_normalize().ok_or("a ribbon's points fold back on themselves")?;
            }
            let s = &mut stations[i];
            s.side = s.tangent.cross(s.up).try_normalize().ok_or("a ribbon cannot run straight up")?;
            s.banks = [-s.side * s.half_width, s.side * s.half_width];
        }
        for (fan, inside, meet) in corners {
            for s in &mut stations[fan] {
                s.banks[inside] = meet;
            }
        }
        for i in 1..stations.len() {
            stations[i].along = stations[i - 1].along + (stations[i].pos - stations[i - 1].pos).length();
        }
        let index = Self::index(&stations, flat);
        let stamp = {
            use std::hash::Hasher;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            h.write_u8(u8::from(flat) | (u8::from(bed == Bed::Flat) << 1) | (u8::from(straight) << 2));
            for p in &points {
                for v in [p.pos.x, p.pos.y, p.pos.z, p.half_width, p.depth, p.current] {
                    h.write_u64(v.to_bits());
                }
            }
            h.finish()
        };
        Ok(Self { flat, bed, straight, points, stations, stamp, index })
    }

    fn index(stations: &[Station], flat: bool) -> Index {
        let pieces = stations.len() - 1;
        let length = stations[pieces].along;
        let widest = stations.iter().fold(0.0f64, |w, s| w.max(s.half_width));
        let radius = (!flat).then(|| stations.iter().map(|s| s.level).sum::<f64>() / stations.len() as f64);
        let mut index = Index {
            cell: (2.0 * length / pieces as f64).max(widest / 4.0).max(1.0),
            radius,
            lo: DVec3::INFINITY,
            hi: DVec3::NEG_INFINITY,
            cells: HashMap::new(),
            pieces: Vec::new(),
        };
        let mut filed: Vec<([i32; 3], u32)> = Vec::new();
        for i in 0..pieces {
            let (a, b) = (&stations[i], &stations[i + 1]);
            // The pieces of a corner's fan have no length: they are there
            // for the strip, and no point is nearest them.
            if a.pos == b.pos {
                continue;
            }
            let (Some(ga), Some(gb)) = (index.ground(a.pos), index.ground(b.pos)) else { continue };
            // Out to the banks, and on a round world a little more: widths
            // are measured at the river's own radius, and a straight piece
            // cuts under the curve of the ground.
            let mut reach = a.half_width.max(b.half_width);
            if let Some(r) = radius {
                let span = (gb - ga).length();
                reach = reach * (r / a.level.min(b.level).max(1e-6)) + span * span / (4.0 * r);
            }
            let reach = reach * 1.001 + 1e-6;
            let reach = if flat { DVec3::new(reach, 0.0, reach) } else { DVec3::splat(reach) };
            let (lo, hi) = (ga.min(gb) - reach, ga.max(gb) + reach);
            index.lo = index.lo.min(lo);
            index.hi = index.hi.max(hi);
            let (k0, k1) = (index.key(lo), index.key(hi));
            for x in k0[0]..=k1[0] {
                for y in k0[1]..=k1[1] {
                    for z in k0[2]..=k1[2] {
                        filed.push(([x, y, z], i as u32));
                    }
                }
            }
        }
        filed.sort_unstable();
        let mut start = 0;
        while start < filed.len() {
            let key = filed[start].0;
            let len = filed[start..].iter().take_while(|(k, _)| *k == key).count();
            index.cells.insert(key, (start as u32, len as u32));
            start += len;
        }
        index.pieces = filed.into_iter().map(|(_, i)| i).collect();
        index
    }

    /// Out of the water at `p`, wet or dry.
    pub fn up_at(&self, p: DVec3) -> DVec3 {
        if self.flat { DVec3::Y } else { p.try_normalize().unwrap_or(DVec3::Y) }
    }

    /// The river's level, as a script reads and sets it: its first point's
    /// surface height.
    pub fn level(&self) -> f64 {
        self.stations[0].level
    }

    /// How far below the surface the bed lies `across` metres from the
    /// middle of a channel `half_width` to its banks and `depth` deep.
    pub fn bed_at(&self, depth: f64, across: f64, half_width: f64) -> f64 {
        match self.bed {
            Bed::Flat => depth,
            Bed::Round if depth.is_infinite() => depth,
            Bed::Round => {
                let x = (across / half_width).clamp(0.0, 1.0);
                depth * (1.0 - x * x)
            }
        }
    }

    /// Where `p` is on the river, when it is between the banks: over it,
    /// in it or under it. `None` beside it and past either end.
    pub fn locate(&self, p: DVec3) -> Option<Located> {
        self.locate_among(p, self.index.near(p))
    }

    /// [`Ribbon::locate`] asking every piece in turn: what the index must
    /// agree with.
    pub fn locate_slowly(&self, p: DVec3) -> Option<Located> {
        let all: Vec<u32> = (0..self.stations.len() as u32 - 1).collect();
        self.locate_among(p, &all)
    }

    fn locate_among(&self, p: DVec3, pieces: &[u32]) -> Option<Located> {
        let (up, height) = if self.flat {
            (DVec3::Y, p.y)
        } else {
            let len = p.length();
            if len < 1e-9 {
                return None;
            }
            (p / len, len)
        };
        let last = self.stations.len() - 2;
        // The nearest place on the centreline whose banks reach this far.
        let mut best: Option<(f64, usize, f64, DVec3)> = None;
        let level = |v: DVec3| v - up * v.dot(up);
        // How far along piece `i` the point is, as a fraction of it.
        let along = |i: usize| {
            let (a, b) = (&self.stations[i], &self.stations[i + 1]);
            let run = level(b.pos - a.pos);
            level(p - a.pos).dot(run) / run.length_squared().max(1e-18)
        };
        let real = |i: usize| self.stations[i].pos != self.stations[i + 1].pos;
        for &i in pieces {
            let i = i as usize;
            let (a, b) = (&self.stations[i], &self.stations[i + 1]);
            // The far side of a round world is not under the river.
            if !real(i) || (!self.flat && a.pos.dot(up) <= 0.0) {
                continue;
            }
            let (run, to) = (level(b.pos - a.pos), level(p - a.pos));
            let t = to.dot(run) / run.length_squared().max(1e-18);
            // Past an end of this piece the point is this piece's only in
            // the wedge on the outside of a turn, between it and the piece
            // it joins there. Anywhere else the next piece answers for it,
            // and past the river's first and last points nothing does: the
            // river ends square.
            if t < 0.0 && (0..i).rev().find(|&j| real(j)).is_none_or(|j| along(j) < 1.0) {
                continue;
            }
            if t > 1.0 && (i + 1..=last).find(|&j| real(j)).is_none_or(|j| along(j) > 0.0) {
                continue;
            }
            let wedge = !(0.0..=1.0).contains(&t);
            let t = t.clamp(0.0, 1.0);
            let off = to - run * t;
            let across = off.length();
            if across <= a.half_width + (b.half_width - a.half_width) * t && best.is_none_or(|(d, ..)| across < d) {
                best = Some((across, i, t, if wedge { off } else { DVec3::ZERO }));
            }
        }
        let (across, i, t, wedge) = best?;
        let (a, b) = (&self.stations[i], &self.stations[i + 1]);
        let mix = |x: f64, y: f64| x + (y - x) * t;
        let half_width = mix(a.half_width, b.half_width);
        let depth = if a.depth.is_infinite() { a.depth } else { mix(a.depth, b.depth) };
        let mut tangent = (a.tangent + (b.tangent - a.tangent) * t).try_normalize().unwrap_or(a.tangent);
        // Round the outside of a turn the water runs round the corner: at
        // right angles to the line from the corner to the point.
        if let Some(round) = up.cross(wedge).try_normalize() {
            tangent = if round.dot(tangent) < 0.0 { -round } else { round };
        }
        Some(Located {
            along: mix(a.along, b.along),
            across,
            half_width,
            level: mix(a.level, b.level),
            height,
            bed: self.bed_at(depth, across, half_width),
            flow: tangent * mix(a.current, b.current),
            up,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f64, y: f64, z: f64, half_width: f64, depth: f64, current: f64) -> RibbonPoint {
        RibbonPoint { pos: DVec3::new(x, y, z), half_width, depth, current }
    }

    /// A river that runs 40 m east, turns a right angle and runs 40 m south
    /// (+z), 3 m to each bank.
    fn bend() -> Ribbon {
        Ribbon::new(
            vec![point(0.0, 0.0, 0.0, 3.0, 2.0, 1.0), point(40.0, 0.0, 0.0, 3.0, 2.0, 1.0), point(40.0, 0.0, 40.0, 3.0, 2.0, 1.0)],
            true,
            Bed::Round,
        )
        .unwrap()
    }

    /// **Between two points every value is between theirs.** From a point 2 m
    /// to its banks, 1 m deep, running 0.5 m/s at height 10, to one 6 m to
    /// its banks, 3 m deep, running 1.5 m/s at height 8: halfway down, the
    /// river is 4 m to its banks, 2 m deep in the middle, runs 1 m/s and its
    /// surface is at 9; a point 3.9 m off the middle is in it there and one
    /// 2.5 m off is not at the narrow end.
    #[test]
    fn a_river_is_eased_between_its_points() {
        let r = Ribbon::new(vec![point(0.0, 10.0, 0.0, 2.0, 1.0, 0.5), point(100.0, 8.0, 0.0, 6.0, 3.0, 1.5)], true, Bed::Round).unwrap();
        let mid = r.locate(DVec3::new(50.0, 0.0, 0.0)).unwrap();
        assert!((mid.half_width - 4.0).abs() < 1e-9 && (mid.level - 9.0).abs() < 1e-9, "{mid:?}");
        assert!((mid.bed - 2.0).abs() < 1e-9 && (mid.flow.length() - 1.0).abs() < 1e-9, "{mid:?}");
        assert!(mid.flow.x > 0.99 && mid.flow.y < 0.0, "it runs downstream and downhill: {:?}", mid.flow);
        assert!((mid.along - 50.0).abs() < 0.1, "{}", mid.along);
        assert!(r.locate(DVec3::new(50.0, 0.0, 3.9)).is_some(), "3.9 m off the middle, 4 m to the bank");
        assert!(r.locate(DVec3::new(50.0, 0.0, 4.1)).is_none(), "past the bank");
        assert!(r.locate(DVec3::new(1.0, 0.0, 2.5)).is_none(), "the narrow end is 2 m to its banks");
        assert!(r.locate(DVec3::new(99.0, 0.0, 5.5)).is_some(), "the wide end is 6 m");
        // Square ends.
        assert!(r.locate(DVec3::new(-0.5, 0.0, 0.0)).is_none() && r.locate(DVec3::new(100.5, 0.0, 0.0)).is_none());
        // A quarter of the way: a quarter of the way between the values.
        let q = r.locate(DVec3::new(25.0, 0.0, 0.0)).unwrap();
        assert!((q.half_width - 3.0).abs() < 1e-9 && (q.level - 9.5).abs() < 1e-9, "{q:?}");
    }

    /// **A value never leaves the range between its points.** A river that
    /// is level, drops 5 m in one stretch and is level again does not rise
    /// before the drop or dip after it, and one that narrows from 6 m to
    /// 0.2 m and stays there never narrows past 0.2.
    #[test]
    fn eased_values_do_not_overshoot() {
        let pts = vec![
            point(0.0, 10.0, 0.0, 6.0, 2.0, 1.0),
            point(50.0, 10.0, 0.0, 6.0, 2.0, 1.0),
            point(60.0, 5.0, 0.0, 0.2, 2.0, 1.0),
            point(110.0, 5.0, 0.0, 0.2, 2.0, 1.0),
            point(160.0, 5.0, 10.0, 0.2, 2.0, 1.0),
        ];
        let r = Ribbon::new(pts, true, Bed::Flat).unwrap();
        for s in &r.stations {
            assert!(s.level <= 10.0 + 1e-9 && s.level >= 5.0 - 1e-9, "the surface left 5..10 at {:?}", s.pos);
            assert!(s.half_width >= 0.2 - 1e-9 && s.half_width <= 6.0 + 1e-9, "the width left 0.2..6: {}", s.half_width);
        }
        for w in r.stations.windows(2) {
            assert!(w[1].level <= w[0].level + 1e-9, "the river runs uphill at {:?}", w[1].pos);
        }
    }

    /// **A bend is whole.** Round a right-angle turn every point within the
    /// half-width of the centreline is in the river, on the outside of the
    /// turn as on the inside, no point further off is, and the distance down
    /// the channel never jumps from one side of the join to the other.
    #[test]
    fn a_bend_has_no_gap_and_no_overlap() {
        let r = bend();
        // The centreline itself, finely: walk it and step off to each side.
        let (mut wet_in, mut dry_out) = (0, 0);
        for w in r.stations.windows(2) {
            for k in 0..8 {
                let t = (k as f64 + 0.5) / 8.0;
                let c = w[0].pos + (w[1].pos - w[0].pos) * t;
                let side = (w[0].side + (w[1].side - w[0].side) * t).normalize();
                for sign in [-1.0, 1.0] {
                    for frac in [0.0, 0.5, 0.97] {
                        let hit = r.locate(c + side * (sign * frac * 3.0));
                        assert!(hit.is_some(), "a gap {frac} of the way to the bank at {c:?}");
                        wet_in += 1;
                    }
                    // Outside the banks of the outside of the turn there is
                    // no river; inside the turn another reach may be near.
                    let out = c + side * (sign * 3.1);
                    let nearest = r.stations.windows(2).map(|s| distance_to(out, s[0].pos, s[1].pos)).fold(f64::INFINITY, f64::min);
                    if nearest > 3.0 + 1e-6 {
                        assert!(r.locate(out).is_none(), "water past the bank at {out:?}");
                        dry_out += 1;
                    }
                }
            }
        }
        assert!(wet_in > 500 && dry_out > 150, "{wet_in} {dry_out}");
        // The corner's outside: a point nearer than 3 m to the curve is wet.
        let corner = r.stations.iter().min_by(|a, b| {
            (a.pos - DVec3::new(40.0, 0.0, 0.0)).length().total_cmp(&(b.pos - DVec3::new(40.0, 0.0, 0.0)).length())
        });
        let c = corner.unwrap();
        assert!(r.locate(c.pos - c.side * 2.9).is_some() && r.locate(c.pos + c.side * 2.9).is_some());
        // Across the whole bend, a walk along a line 2 m outside the
        // centreline sees `along` only ever grow, by no more than the walk.
        let mut before: Option<f64> = None;
        for w in r.stations.windows(2) {
            for k in 0..4 {
                let t = (k as f64 + 0.5) / 4.0;
                let side = (w[0].side + (w[1].side - w[0].side) * t).normalize();
                // −side is the outside of this turn (east then south).
                let at = r.locate(w[0].pos + (w[1].pos - w[0].pos) * t - side * 2.0).unwrap().along;
                if let Some(b) = before {
                    assert!(at >= b - 1e-6 && at - b < 3.0, "along jumped from {b} to {at}");
                }
                before = Some(at);
            }
        }
    }

    /// **A straight river is its points joined by straight lines, with every
    /// corner whole.** Its middle never leaves the lines; at a right-angle
    /// corner the outside is rounded (every point within the half-width of
    /// the corner is water, and the water runs round it), the inside banks
    /// meet at one point, and in the river is exactly within the half-width
    /// of the lines and between its two ends.
    #[test]
    fn a_straight_river_rounds_the_outside_of_a_corner() {
        let r = Ribbon::with(bend().points, true, Bed::Round, true).unwrap();
        for s in &r.stations {
            assert!(s.pos.z.abs() < 1e-9 || (s.pos.x - 40.0).abs() < 1e-9, "the middle left the lines at {:?}", s.pos);
        }
        let (a, b, c) = (DVec3::ZERO, DVec3::new(40.0, 0.0, 0.0), DVec3::new(40.0, 0.0, 40.0));
        let (mut wet, mut dry) = (0, 0);
        for i in 0..=200 {
            for j in 0..=200 {
                let p = DVec3::new(-5.0 + i as f64 * 0.25 + 0.01, 0.0, -5.0 + j as f64 * 0.25 + 0.01);
                let d = distance_to(p, a, b).min(distance_to(p, b, c));
                let want = d < 3.0 && p.x > 0.0 && p.z < 40.0;
                assert_eq!(r.locate(p).is_some(), want, "at {p:?}, {d} m from the lines");
                if want { wet += 1 } else { dry += 1 }
            }
        }
        assert!(wet > 7_000 && dry > 30_000, "{wet} {dry}");
        // Outside the corner, on its diagonal: at the corner's distance down
        // the river, running round it.
        let out = r.locate(DVec3::new(42.0, 0.0, -2.0)).unwrap();
        assert!((out.along - 40.0).abs() < 1e-9 && (out.across - 8f64.sqrt()).abs() < 1e-9, "{out:?}");
        assert!((out.flow - DVec3::new(1.0, 0.0, 1.0).normalize()).length() < 1e-9, "{:?}", out.flow);
        // The fan at the corner: one place, headings from east to south,
        // outside banks on the round and inside banks at (37, 3).
        let fan: Vec<&Station> = r.stations.iter().filter(|s| s.pos == b).collect();
        assert!(fan.len() >= 16, "{} stations round a right angle", fan.len());
        assert!((fan[0].tangent - DVec3::X).length() < 1e-9 && (fan[fan.len() - 1].tangent - DVec3::Z).length() < 1e-9);
        for s in fan {
            assert!((s.banks[0].length() - 3.0).abs() < 1e-9 && s.banks[0].x >= -1e-9 && s.banks[0].z <= 1e-9, "{:?}", s.banks[0]);
            assert!((s.pos + s.banks[1] - DVec3::new(37.0, 0.0, 3.0)).length() < 1e-9, "{:?}", s.banks[1]);
        }
        // Everywhere else a bank is the half-width off, square to the flow.
        let s = &r.stations[1];
        assert_eq!(s.banks, [DVec3::new(0.0, 0.0, -3.0), DVec3::new(0.0, 0.0, 3.0)]);
    }

    fn distance_to(p: DVec3, a: DVec3, b: DVec3) -> f64 {
        let t = ((p - a).dot(b - a) / (b - a).length_squared()).clamp(0.0, 1.0);
        (p - a - (b - a) * t).length()
    }

    /// **The bed shallows to the banks, or does not.** A gentle U is the full
    /// depth in the middle, three quarters of it halfway to the bank and
    /// nothing at the bank; a flat bed is the same depth from bank to bank;
    /// and a river declared with no depth has no bed at all.
    #[test]
    fn the_bed_is_a_gentle_u_or_flat() {
        let pts = || vec![point(0.0, 0.0, 0.0, 4.0, 2.0, 0.0), point(50.0, 0.0, 0.0, 4.0, 2.0, 0.0)];
        let u = Ribbon::new(pts(), true, Bed::Round).unwrap();
        let bed = |r: &Ribbon, z: f64| r.locate(DVec3::new(25.0, 0.0, z)).unwrap().bed;
        assert!((bed(&u, 0.0) - 2.0).abs() < 1e-9 && (bed(&u, 2.0) - 1.5).abs() < 1e-9 && bed(&u, 3.999) < 0.01);
        let flat = Ribbon::new(pts(), true, Bed::Flat).unwrap();
        assert!((bed(&flat, 0.0) - 2.0).abs() < 1e-9 && (bed(&flat, 3.9) - 2.0).abs() < 1e-9);
        let mut open = pts();
        open[0].depth = f64::INFINITY;
        let open = Ribbon::new(open, true, Bed::Round).unwrap();
        assert!(bed(&open, 3.0).is_infinite());
    }

    /// A long winding river: `points` of them 12 m apart, swinging from side
    /// to side.
    fn winding(points: usize) -> Ribbon {
        let pts = (0..points)
            .map(|i| {
                let s = i as f64 * 12.0;
                point(s, -0.01 * s, 30.0 * (s / 90.0).sin(), 3.0 + (i % 5) as f64 * 0.5, 2.0, 1.0)
            })
            .collect();
        Ribbon::new(pts, true, Bed::Round).unwrap()
    }

    /// Points scattered over and beside a river, some in it and some not.
    fn probes(r: &Ribbon, count: usize) -> Vec<DVec3> {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..count)
            .map(|_| {
                let s = &r.stations[(next() * r.stations.len() as f64) as usize % r.stations.len()];
                s.pos + s.side * ((next() * 2.0 - 1.0) * 2.0 * s.half_width) + s.tangent * (next() * 4.0 - 2.0) + s.up * (next() * 6.0 - 4.0)
            })
            .collect()
    }

    /// **The index answers as asking every piece does.** Over a winding
    /// river of 300 points, on a plane and on a round world, twenty thousand
    /// points in and beside the water get the same answer from the index as
    /// from a walk of the whole river.
    #[test]
    fn the_index_agrees_with_asking_every_piece() {
        let flat = winding(300);
        // The same river wrapped on a world of radius 900.
        let round = Ribbon::new(
            flat.points
                .iter()
                .map(|p| {
                    let (a, b) = (p.pos.x / 900.0, p.pos.z / 900.0);
                    let dir = DVec3::new(a.sin() * b.cos(), a.cos() * b.cos(), b.sin());
                    RibbonPoint { pos: dir * (900.0 + p.pos.y), ..*p }
                })
                .collect(),
            false,
            Bed::Round,
        )
        .unwrap();
        for r in [&flat, &round] {
            let mut wet = 0;
            for p in probes(r, 20_000) {
                let (fast, slow) = (r.locate(p), r.locate_slowly(p));
                assert_eq!(fast, slow, "at {p:?}");
                wet += usize::from(fast.is_some());
            }
            assert!(wet > 6_000 && wet < 14_000, "{wet} of 20,000 probes were in the river");
        }
    }

    /// **A river on a round world is measured along the world.** Up is the
    /// radial, a height is a distance from the centre, the banks are the
    /// half-width off along the ground whatever height the question is asked
    /// from, and the current runs along the ground.
    #[test]
    fn a_river_on_a_round_world_follows_it() {
        let at = |a: f64, r: f64| DVec3::new(a.sin(), a.cos(), 0.0) * r;
        let river = Ribbon::new(
            vec![
                RibbonPoint { pos: at(0.0, 500.0), half_width: 4.0, depth: 2.0, current: 1.0 },
                RibbonPoint { pos: at(0.2, 500.0), half_width: 4.0, depth: 2.0, current: 1.0 },
                RibbonPoint { pos: at(0.4, 498.0), half_width: 4.0, depth: 2.0, current: 1.0 },
            ],
            false,
            Bed::Round,
        )
        .unwrap();
        // A fifth of the way round the first stretch, 1 m under and 30 m over.
        for height in [499.0, 530.0] {
            let p = at(0.1, height);
            let hit = river.locate(p).unwrap();
            assert!((hit.level - 500.0).abs() < 1e-6 && (hit.height - height).abs() < 1e-9, "{hit:?}");
            assert!((hit.up - p.normalize()).length() < 1e-12);
            assert!(hit.flow.dot(hit.up).abs() < 1e-3 && (hit.flow.length() - 1.0).abs() < 1e-9, "{:?}", hit.flow);
            // The banks, measured at the river: 3.9 m off is in, 4.1 is out.
            let off = |m: f64| p + DVec3::Z * (m * height / 500.0);
            assert!(river.locate(off(3.9)).is_some(), "3.9 m off at height {height}");
            assert!(river.locate(off(4.1)).is_none(), "4.1 m off at height {height}");
        }
        // The far side of the world is not under the river.
        assert!(river.locate(at(0.1, -499.0)).is_none());
        // Laid at the centre of its parent, it says why.
        let err = Ribbon::new(vec![point(0.0, 0.0, 0.0, 2.0, 1.0, 0.0), point(10.0, 0.0, 0.0, 2.0, 1.0, 0.0)], false, Bed::Round).unwrap_err();
        assert!(err.contains("flat = true"), "{err}");
    }

    /// **A question costs the same on a long river as on a short one.** A
    /// river of 1,600 points answers a point in 2× what one of 100 points
    /// does or less, where a walk of every piece takes sixteen times as
    /// long. Measured as a ratio, so the machine's speed does not matter.
    #[test]
    fn a_question_does_not_cost_more_on_a_longer_river() {
        let time = |r: &Ribbon, slowly: bool| {
            let ps = probes(r, 4_000);
            let mut best = f64::INFINITY;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                let mut wet = 0usize;
                for p in &ps {
                    let hit = if slowly { r.locate_slowly(*p) } else { r.locate(*p) };
                    wet += usize::from(std::hint::black_box(hit).is_some());
                }
                assert!(wet > 0);
                best = best.min(t.elapsed().as_secs_f64() / ps.len() as f64);
            }
            best
        };
        let (short, long) = (winding(100), winding(1_600));
        assert!(long.stations.len() > 12 * short.stations.len());
        let (a, b) = (time(&short, false), time(&long, false));
        assert!(b < a * 2.0, "a point costs {:.0} ns on 100 points and {:.0} ns on 1,600", a * 1e9, b * 1e9);
        // The walk the index replaces does grow with the river.
        let (c, d) = (time(&short, true), time(&long, true));
        assert!(d > c * 6.0, "asking every piece cost {:.0} ns and {:.0} ns", c * 1e9, d * 1e9);
    }
}
