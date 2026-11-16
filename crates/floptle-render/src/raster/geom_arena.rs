//! **Shared vertex and index buffers for dynamic mesh slots.**
//!
//! Every terrain chunk is a dynamic slot, and each slot used to own a vertex
//! buffer and an index buffer of its own. So every chunk drawn in every pass
//! set both again, for real: they were different objects, and the GPU
//! library validated and tracked each set when the pass ended (card 0753).
//! Here the slots take their ranges out of a few large buffers instead, and
//! a run of chunks binds them once.
//!
//! A page is one vertex buffer and one index buffer, each carved up by a
//! buddy allocator: a slot's capacity is already a power of two
//! (`slot_cap`), so a range is a buddy block, and freeing one merges it with
//! its free neighbour on the spot. A page starts small and doubles (one copy
//! on the GPU) until it reaches the most a buffer may hold; then a new page
//! starts. A page with nothing left in it is let go, so the memory comes
//! back when a planet unloads.
//!
//! A slot's indices are written already offset by its first vertex, so a
//! draw needs no base vertex: that is not on every backend (WebGL2 has
//! none), and `@builtin(vertex_index)` is then the vertex's place in the
//! page wherever it runs. The raster shader reads four stores by
//! `vertex_index`, counting from the mesh's first vertex: terrain colours,
//! vertex paint, slot tables and skinning. For terrain colours the word in
//! front of each colour block holds the slot's first vertex in the page,
//! and the shader takes it off (see `Raster::write_tpaint_first`). The other
//! three have nothing to correct them, so only slots with a colour block
//! come here (terrain chunks and decals, whose instances read none of the
//! three); the rest keep buffers of their own.

use super::*;
use std::collections::{BTreeSet, HashMap};

/// The least a range can be: `EMPTY_SLOT_CAP`.
const MIN_BLOCK: u32 = 4;

/// A buddy allocator over `0..size`, every block a power of two from
/// [`MIN_BLOCK`] up.
#[derive(Debug)]
pub(super) struct Buddy {
    size: u32,
    /// Free blocks by order (`MIN_BLOCK << order` long), by offset, so a
    /// block comes from as low in the page as it can.
    free: Vec<BTreeSet<u32>>,
    used: u32,
}

fn order_of(n: u32) -> usize {
    (n.max(MIN_BLOCK).trailing_zeros() - MIN_BLOCK.trailing_zeros()) as usize
}

impl Buddy {
    pub(super) fn new(size: u32) -> Self {
        debug_assert!(size.is_power_of_two() && size >= MIN_BLOCK);
        let mut b = Buddy { size, free: vec![BTreeSet::new(); order_of(size) + 1], used: 0 };
        b.free[order_of(size)].insert(0);
        b
    }

    pub(super) fn size(&self) -> u32 {
        self.size
    }

    /// Elements handed out.
    pub(super) fn used(&self) -> u32 {
        self.used
    }

    /// A block of `n` (a power of two), or `None` when nothing that large
    /// is free.
    pub(super) fn alloc(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n.is_power_of_two());
        let want = order_of(n);
        let (from, off) = (want..self.free.len()).find_map(|o| self.free[o].first().map(|&off| (o, off)))?;
        self.free[from].remove(&off);
        // Split down to size, the upper halves going back free.
        for o in (want..from).rev() {
            self.free[o].insert(off + (MIN_BLOCK << o));
        }
        self.used += MIN_BLOCK << want;
        Some(off)
    }

    /// Give back the block of `n` at `off`, merged with its free buddy, and
    /// that pair with theirs, as far as they go.
    pub(super) fn free(&mut self, mut off: u32, n: u32) {
        self.used -= n.max(MIN_BLOCK);
        let mut o = order_of(n);
        while (MIN_BLOCK << o) < self.size {
            let buddy = off ^ (MIN_BLOCK << o);
            if !self.free[o].remove(&buddy) {
                break;
            }
            off = off.min(buddy);
            o += 1;
        }
        self.free[o].insert(off);
    }

    /// How many free blocks there are of each order: all a dry run of
    /// [`Self::alloc`] needs ([`take_count`]).
    fn free_counts(&self) -> Vec<u32> {
        self.free.iter().map(|s| s.len() as u32).collect()
    }

    /// Double the range: the new upper half is one free block (merged with
    /// the lower half if all of that is free too).
    pub(super) fn grow(&mut self) {
        let old = self.size;
        self.size = old * 2;
        self.free.push(BTreeSet::new());
        self.used += old;
        self.free(old, old);
    }
}

/// [`Buddy::alloc`] of `n` played on free-block counts (by order): the
/// block it would take, split the same way, without touching the free
/// lists. `None` when no block is large enough.
fn take_count(counts: &mut [u32], n: u32) -> Option<usize> {
    let want = order_of(n);
    let from = (want..counts.len()).find(|&o| counts[o] > 0)?;
    counts[from] -= 1;
    for c in &mut counts[want..from] {
        *c += 1;
    }
    Some(from)
}

/// Undo [`take_count`]: the upper halves it split off go back, then the
/// block it took.
fn untake_count(counts: &mut [u32], n: u32, from: usize) {
    let want = order_of(n);
    for c in &mut counts[want..from] {
        *c -= 1;
    }
    counts[from] += 1;
}

/// Whether a buffer of `size` with `used` handed out has room for `want`
/// once doubled as often as `max` allows (free space is not always one
/// block, so this is where growing starts, not a promise).
fn could_hold(size: u32, used: u32, want: u32, max: u32) -> bool {
    let mut s = size;
    while s - used < want && s < max {
        s *= 2;
    }
    s - used >= want
}

/// Which of a page's buffers to double for a slot, each given as (size,
/// used, most, wanted): those short of room for it (or smaller than twice
/// it), else both that can; `None` when neither can grow.
fn grow_choice(v: (u32, u32, u32, u32), i: (u32, u32, u32, u32)) -> Option<(bool, bool)> {
    let short = |(size, used, _, want): (u32, u32, u32, u32)| size - used < want || size < want * 2;
    let gv = v.0 < v.2 && short(v);
    let gi = i.0 < i.2 && short(i);
    let (gv, gi) = if gv || gi { (gv, gi) } else { (v.0 < v.2, i.0 < i.2) };
    (gv || gi).then_some((gv, gi))
}

/// A [`Buddy`] reduced to what a dry run of taking blocks and growing
/// needs: its size, what is handed out, and its free blocks' counts.
struct SimBuddy {
    size: u32,
    used: u32,
    counts: Vec<u32>,
}

impl SimBuddy {
    fn of(b: &Buddy) -> Self {
        SimBuddy { size: b.size(), used: b.used(), counts: b.free_counts() }
    }

    /// [`GeomArena::take`]: a vertex block and an index block, or neither.
    fn take_pair(v: &mut SimBuddy, i: &mut SimBuddy, v_cap: u32, i_cap: u32) -> bool {
        let Some(vf) = take_count(&mut v.counts, v_cap) else { return false };
        if take_count(&mut i.counts, i_cap).is_none() {
            untake_count(&mut v.counts, v_cap, vf);
            return false;
        }
        v.used += v_cap.max(MIN_BLOCK);
        i.used += i_cap.max(MIN_BLOCK);
        true
    }

    /// [`Buddy::grow`]: the new upper half is a free block, merged with the
    /// lower half when that is one free block too.
    fn grow(&mut self) {
        let o = order_of(self.size);
        self.size *= 2;
        if self.counts[o] > 0 {
            self.counts[o] -= 1;
            self.counts.push(1);
        } else {
            self.counts[o] += 1;
            self.counts.push(0);
        }
    }
}

/// The page [`GeomArena::victim`] chose to empty.
#[derive(Clone, Copy, Debug)]
pub(super) struct Victim {
    pub page: u32,
    /// Its slots may go to a new page when the others have no room.
    pub new_pages: bool,
}

/// Where a slot's geometry lives in the arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ArenaRange {
    pub page: u32,
    /// First vertex, and room for how many.
    pub v_off: u32,
    pub v_cap: u32,
    /// First index, and room for how many.
    pub i_off: u32,
    pub i_cap: u32,
}

struct Page {
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    verts: Buddy,
    indices: Buddy,
    /// Slots holding a range here.
    live: u32,
}

/// The pages. See the module docs.
pub(super) struct GeomArena {
    pages: Vec<Option<Page>>,
    /// The most one page's buffers may hold, in vertices and in indices.
    max_verts: u32,
    max_indices: u32,
    /// Bytes written into the pages since made: slot writes and the copies
    /// a page makes when it grows.
    pub written: u64,
    /// Bytes of slot geometry moved out of emptying pages since made
    /// ([`Self::victim`]).
    pub moved: u64,
    /// Bumped whenever a range is taken or given back or a page comes or
    /// goes, so compaction can tell nothing has changed since it last found
    /// nothing to do, and do nothing again for free.
    pub(super) changes: u64,
    /// The page being emptied: new slots go elsewhere while anywhere else
    /// has room, so the room its slots leave is not taken straight back.
    draining: Option<u32>,
}

pub(super) const VERTEX_BYTES: u64 = std::mem::size_of::<Vertex>() as u64;

/// A new page starts this large (512 KiB of vertices, 128 KiB of indices),
/// so a scene with a sprite or two does not hold a planet's worth.
const FIRST_PAGE_VERTS: u32 = 1 << 14;
const FIRST_PAGE_INDICES: u32 = 1 << 15;

/// The pages hold more than this many times what is live before a page
/// is emptied into the others' room ([`GeomArena::victim`]). Packed
/// tighter, the next chunk to arrive would grow a page straight back.
const COMPACT_ABOVE: f64 = 1.2;

fn page_buffer(gpu: &Gpu, label: &str, bytes: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes,
        usage: usage | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

impl GeomArena {
    pub(super) fn new(gpu: &Gpu) -> Self {
        // A page fills one buffer at most: 64 MiB of vertices and 32 MiB of
        // indices, or the device's limit if that is less.
        let limit = gpu.device.limits().max_buffer_size;
        // A device whose buffers cannot hold a first page gets no arena at
        // all (0): every slot keeps buffers of its own, as before.
        let fit = |per: u64, cap: u32, first: u32| -> u32 {
            let n = (limit / per).min(cap as u64) as u32;
            let n = if n.is_power_of_two() { n } else { n.next_power_of_two() / 2 };
            if n < first { 0 } else { n }
        };
        let (max_verts, max_indices) = (fit(VERTEX_BYTES, 1 << 21, FIRST_PAGE_VERTS), fit(4, 1 << 23, FIRST_PAGE_INDICES));
        let (max_verts, max_indices) = if max_verts == 0 || max_indices == 0 { (0, 0) } else { (max_verts, max_indices) };
        GeomArena { pages: Vec::new(), max_verts, max_indices, written: 0, moved: 0, changes: 0, draining: None }
    }

    /// Hold a page to at most `verts` vertices and `indices` indices (each
    /// a power of two no smaller than a first page, and no larger than the
    /// device allows), so a test can fill several pages with little.
    pub(super) fn limit_pages(&mut self, verts: u32, indices: u32) {
        let fit = |n: u32, first: u32, max: u32| n.next_power_of_two().max(first).min(max);
        self.max_verts = fit(verts, FIRST_PAGE_VERTS, self.max_verts);
        self.max_indices = fit(indices, FIRST_PAGE_INDICES, self.max_indices);
    }

    /// Bytes the pages hold on the GPU.
    pub(super) fn bytes(&self) -> u64 {
        self.pages.iter().flatten().map(|p| p.vbuf.size() + p.ibuf.size()).sum()
    }

    /// Bytes the slots' ranges take up in the pages: what is live, against
    /// [`Self::bytes`], what is held.
    pub(super) fn live_bytes(&self) -> u64 {
        self.pages.iter().flatten().map(|p| p.verts.used() as u64 * VERTEX_BYTES + p.indices.used() as u64 * 4).sum()
    }

    /// A page worth emptying: grown past a first page's size, with its live
    /// ranges filling less than a quarter of its vertex buffer and of its
    /// index buffer. The emptiest such page. Its slots move to other pages
    /// (or a small new one) and it is let go: a page only grows, and the
    /// coarse chunks left on it after the player has walked on would
    /// otherwise hold it at its peak until the planet unloads. A page filled
    /// to a quarter or more is not one, so the slots moved into another page
    /// do not move again.
    ///
    /// Short of that, while the pages hold more than [`COMPACT_ABOVE`] times
    /// what is live, a page whose slots all fit in the other pages, in the
    /// room they have or by growing them by less than the page itself
    /// holds: the emptiest such page. On a world small enough to be drawn
    /// whole, far ground stays on every page, and a walk round it grows
    /// each page in turn without any emptying to a quarter; moving one
    /// page's slots into the others and letting it go gives memory back
    /// without starting a page to do it. `ranges` is each page's slots'
    /// capacities, vertices and indices, largest first.
    ///
    /// The page chosen is the one being emptied ([`Self::draining`]) until
    /// it goes or no longer qualifies. With it, whether its slots may go to
    /// a new page ([`Victim::new_pages`]): only for the first kind, whose
    /// slots are few for its size.
    pub(super) fn victim(&mut self, ranges: &HashMap<u32, Vec<(u32, u32)>>) -> Option<Victim> {
        let mut best: Option<(u32, f64)> = None;
        for (i, p) in self.pages.iter().enumerate() {
            let Some(p) = p else { continue };
            if p.verts.size() <= FIRST_PAGE_VERTS && p.indices.size() <= FIRST_PAGE_INDICES {
                continue;
            }
            let full = Self::full(p);
            if full < 0.25 && best.is_none_or(|(_, b)| full < b) {
                best = Some((i as u32, full));
            }
        }
        if best.is_none() && self.bytes() as f64 > self.live_bytes() as f64 * COMPACT_ABOVE {
            // The page being emptied first, then the emptiest first.
            let mut by_room: Vec<(u32, f64)> =
                self.pages.iter().enumerate().filter_map(|(i, p)| Some((i as u32, Self::full(p.as_ref()?)))).collect();
            by_room.sort_by(|a, b| (Some(b.0) == self.draining).cmp(&(Some(a.0) == self.draining)).then(a.1.total_cmp(&b.1)));
            let found = by_room.into_iter().find(|&(i, _)| {
                let size = self.pages[i as usize].as_ref().map_or(0, |p| p.vbuf.size() + p.ibuf.size());
                self.grows_to_take(i, ranges.get(&i).map_or(&[][..], Vec::as_slice)).is_some_and(|added| added < size)
            });
            self.draining = found.map(|(i, _)| i);
            return found.map(|(page, _)| Victim { page, new_pages: false });
        }
        self.draining = best.map(|(i, _)| i);
        best.map(|(page, _)| Victim { page, new_pages: true })
    }

    /// How full a page is: the fuller of its two buffers, as a share.
    fn full(p: &Page) -> f64 {
        (p.verts.used() as f64 / p.verts.size() as f64).max(p.indices.used() as f64 / p.indices.size() as f64)
    }

    /// The pages other than `avoid`, fullest first: where moved slots go,
    /// so they fill the pages least likely to be emptied next.
    pub(super) fn fullest_first(&self, avoid: u32) -> Vec<u32> {
        let mut out: Vec<(u32, f64)> = self
            .pages
            .iter()
            .enumerate()
            .filter(|&(i, _)| i as u32 != avoid)
            .filter_map(|(i, p)| Some((i as u32, Self::full(p.as_ref()?))))
            .collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out.into_iter().map(|(i, _)| i).collect()
    }

    /// The bytes the pages other than `page` would grow by to take every
    /// range in `ranges` (largest first), each placed as [`Self::place`]
    /// places it; `None` when they cannot take them all. A dry run on the
    /// free blocks' counts, which touches no page.
    fn grows_to_take(&self, page: u32, ranges: &[(u32, u32)]) -> Option<u64> {
        let order = self.fullest_first(page);
        let mut sim: Vec<(SimBuddy, SimBuddy)> = order
            .iter()
            .map(|&i| {
                let p = self.pages[i as usize].as_ref().expect("a live page");
                (SimBuddy::of(&p.verts), SimBuddy::of(&p.indices))
            })
            .collect();
        let before: u64 = sim.iter().map(|(v, i)| v.size as u64 * VERTEX_BYTES + i.size as u64 * 4).sum();
        let (max_v, max_i) = (self.max_verts, self.max_indices);
        for &(v, ix) in ranges {
            if sim.iter_mut().any(|(sv, si)| SimBuddy::take_pair(sv, si, v, ix)) {
                continue;
            }
            let at = sim.iter().position(|(sv, si)| could_hold(sv.size, sv.used, v, max_v) && could_hold(si.size, si.used, ix, max_i))?;
            let (sv, si) = &mut sim[at];
            let mut took = false;
            while let Some((gv, gi)) = grow_choice((sv.size, sv.used, max_v, v), (si.size, si.used, max_i, ix)) {
                if gv {
                    sv.grow();
                }
                if gi {
                    si.grow();
                }
                if SimBuddy::take_pair(sv, si, v, ix) {
                    took = true;
                    break;
                }
            }
            if !took {
                return None;
            }
        }
        let after: u64 = sim.iter().map(|(v, i)| v.size as u64 * VERTEX_BYTES + i.size as u64 * 4).sum();
        Some(after - before)
    }

    /// Room for a slot being moved, in the pages in `order`
    /// ([`Self::fullest_first`]): where one already has it, else by growing
    /// the first that could hold it once grown. Never a new page. The page
    /// that grew, if one did, has new buffers: every slot on it must take
    /// them up, whatever the answer.
    pub(super) fn place(&mut self, gpu: &Gpu, v_cap: u32, i_cap: u32, order: &[u32]) -> (Option<ArenaRange>, Option<u32>) {
        if let Some(r) = self.alloc_existing(v_cap, i_cap, order) {
            return (Some(r), None);
        }
        let (max_v, max_i) = (self.max_verts, self.max_indices);
        let at = order.iter().copied().find(|&i| {
            self.pages.get(i as usize).and_then(Option::as_ref).is_some_and(|p| {
                could_hold(p.verts.size(), p.verts.used(), v_cap, max_v) && could_hold(p.indices.size(), p.indices.used(), i_cap, max_i)
            })
        });
        let Some(at) = at else { return (None, None) };
        let mut grown = None;
        while self.grow(gpu, at as usize, v_cap, i_cap) {
            grown = Some(at);
            if let Some(r) = self.alloc_in(at, v_cap, i_cap) {
                return (Some(r), grown);
            }
        }
        (None, grown)
    }

    /// Let go of every page no slot holds a range in. `free` drops a page
    /// its last slot leaves; this also catches one that never held a slot.
    pub(super) fn drop_empty(&mut self) {
        for p in &mut self.pages {
            if p.as_ref().is_some_and(|p| p.live == 0) {
                *p = None;
                self.changes += 1;
            }
        }
    }

    /// Room in a page that already has it, without growing one or starting
    /// one, trying the pages in `order` ([`Self::fullest_first`]).
    pub(super) fn alloc_existing(&mut self, v_cap: u32, i_cap: u32, order: &[u32]) -> Option<ArenaRange> {
        for &i in order {
            let Some(p) = self.pages.get_mut(i as usize).and_then(Option::as_mut) else { continue };
            if let Some(r) = Self::take(p, i, v_cap, i_cap) {
                self.changes += 1;
                return Some(r);
            }
        }
        None
    }

    /// Start a page with room for `verts` vertices and `indices` indices
    /// (the next power of two, at least a first page, at most a page's
    /// limit), and return it.
    pub(super) fn new_page(&mut self, gpu: &Gpu, verts: u32, indices: u32) -> u32 {
        let verts = verts.next_power_of_two().max(FIRST_PAGE_VERTS).min(self.max_verts);
        let indices = indices.next_power_of_two().max(FIRST_PAGE_INDICES).min(self.max_indices);
        let p = Page {
            vbuf: page_buffer(gpu, "raster-arena-verts", verts as u64 * VERTEX_BYTES, wgpu::BufferUsages::VERTEX),
            ibuf: page_buffer(gpu, "raster-arena-indices", indices as u64 * 4, wgpu::BufferUsages::INDEX),
            verts: Buddy::new(verts),
            indices: Buddy::new(indices),
            live: 0,
        };
        self.changes += 1;
        match self.pages.iter().position(Option::is_none) {
            Some(at) => {
                self.pages[at] = Some(p);
                at as u32
            }
            None => {
                self.pages.push(Some(p));
                self.pages.len() as u32 - 1
            }
        }
    }

    /// Room in page `page` only.
    pub(super) fn alloc_in(&mut self, page: u32, v_cap: u32, i_cap: u32) -> Option<ArenaRange> {
        let p = self.pages.get_mut(page as usize)?.as_mut()?;
        let r = Self::take(p, page, v_cap, i_cap)?;
        self.changes += 1;
        Some(r)
    }

    /// Pages in use.
    pub(super) fn pages(&self) -> usize {
        self.pages.iter().flatten().count()
    }

    /// A page's buffers.
    pub(super) fn buffers(&self, page: u32) -> (&wgpu::Buffer, &wgpu::Buffer) {
        let p = self.pages[page as usize].as_ref().expect("a live arena page");
        (&p.vbuf, &p.ibuf)
    }

    /// Room for `v_cap` vertices and `i_cap` indices (powers of two), or
    /// `None` when one page could never hold it; and the page that grew on
    /// the way, if one did, whose buffers are new: every slot on it must
    /// take them up, whatever the answer.
    ///
    /// `avoid` is a page to leave alone: one being emptied.
    pub(super) fn alloc(&mut self, gpu: &Gpu, v_cap: u32, i_cap: u32, avoid: Option<u32>) -> (Option<ArenaRange>, Option<u32>) {
        if v_cap > self.max_verts || i_cap > self.max_indices {
            return (None, None);
        }
        self.changes += 1;
        // The page being emptied last of the pages with room, and never
        // grown: what its slots leave is to go, not to be filled again.
        let draining = self.draining.filter(|&d| avoid != Some(d));
        let order = (0..self.pages.len() as u32).filter(|&i| Some(i) != draining).chain(draining);
        for i in order {
            let Some(p) = self.pages[i as usize].as_mut().filter(|_| avoid != Some(i)) else { continue };
            if let Some(r) = Self::take(p, i, v_cap, i_cap) {
                return (Some(r), None);
            }
        }
        // Grow the first page that could hold it once grown. Free space is
        // not always one block, so it grows until the ranges are there or
        // it is as large as a page gets.
        let fits = |b: &Buddy, want: u32, max: u32| could_hold(b.size(), b.used(), want, max);
        let grower = self.pages.iter().enumerate().position(|(i, p)| {
            avoid != Some(i as u32)
                && self.draining != Some(i as u32)
                && p.as_ref().is_some_and(|p| fits(&p.verts, v_cap, self.max_verts) && fits(&p.indices, i_cap, self.max_indices))
        });
        let mut grown = None;
        if let Some(i) = grower {
            while self.grow(gpu, i, v_cap, i_cap) {
                grown = Some(i as u32);
                let p = self.pages[i].as_mut().expect("page");
                if let Some(r) = Self::take(p, i as u32, v_cap, i_cap) {
                    return (Some(r), grown);
                }
            }
        }
        // A new page, as large as this asks for.
        let verts = FIRST_PAGE_VERTS.max(v_cap).min(self.max_verts);
        let indices = FIRST_PAGE_INDICES.max(i_cap).min(self.max_indices);
        let mut p = Page {
            vbuf: page_buffer(gpu, "raster-arena-verts", verts as u64 * VERTEX_BYTES, wgpu::BufferUsages::VERTEX),
            ibuf: page_buffer(gpu, "raster-arena-indices", indices as u64 * 4, wgpu::BufferUsages::INDEX),
            verts: Buddy::new(verts),
            indices: Buddy::new(indices),
            live: 0,
        };
        let at = self.pages.iter().position(Option::is_none).unwrap_or(self.pages.len());
        let r = Self::take(&mut p, at as u32, v_cap, i_cap).expect("a new page holds what it was sized for");
        if at == self.pages.len() {
            self.pages.push(Some(p));
        } else {
            self.pages[at] = Some(p);
        }
        (Some(r), grown)
    }

    fn take(p: &mut Page, page: u32, v_cap: u32, i_cap: u32) -> Option<ArenaRange> {
        let v_off = p.verts.alloc(v_cap)?;
        let Some(i_off) = p.indices.alloc(i_cap) else {
            p.verts.free(v_off, v_cap);
            return None;
        };
        p.live += 1;
        Some(ArenaRange { page, v_off, v_cap, i_off, i_cap })
    }

    /// Double whichever of page `i`'s buffers lacks room for the ask,
    /// copying what it holds into the new one. `false` when neither can.
    fn grow(&mut self, gpu: &Gpu, i: usize, v_cap: u32, i_cap: u32) -> bool {
        let (max_v, max_i) = (self.max_verts, self.max_indices);
        let p = self.pages[i].as_mut().expect("page");
        let Some((grow_v, grow_i)) =
            grow_choice((p.verts.size(), p.verts.used(), max_v, v_cap), (p.indices.size(), p.indices.used(), max_i, i_cap))
        else {
            return false;
        };
        self.changes += 1;
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("arena-grow") });
        if grow_v {
            let nb = page_buffer(gpu, "raster-arena-verts", p.vbuf.size() * 2, wgpu::BufferUsages::VERTEX);
            enc.copy_buffer_to_buffer(&p.vbuf, 0, &nb, 0, p.vbuf.size());
            self.written += p.vbuf.size();
            p.vbuf = nb;
            p.verts.grow();
        }
        if grow_i {
            let nb = page_buffer(gpu, "raster-arena-indices", p.ibuf.size() * 2, wgpu::BufferUsages::INDEX);
            enc.copy_buffer_to_buffer(&p.ibuf, 0, &nb, 0, p.ibuf.size());
            self.written += p.ibuf.size();
            p.ibuf = nb;
            p.indices.grow();
        }
        // Now, ahead of any write into the new buffers: a write queued
        // before this submit lands in the old buffer first and is copied.
        gpu.queue.submit([enc.finish()]);
        true
    }

    /// Give a range back. `true` when its page emptied and was let go.
    pub(super) fn free(&mut self, r: ArenaRange) -> bool {
        let Some(p) = self.pages.get_mut(r.page as usize).and_then(Option::as_mut) else { return false };
        p.verts.free(r.v_off, r.v_cap);
        p.indices.free(r.i_off, r.i_cap);
        p.live -= 1;
        self.changes += 1;
        if p.live == 0 {
            self.pages[r.page as usize] = None;
            return true;
        }
        false
    }

    /// Write a slot's mesh into its range: the vertices as they are, the
    /// indices offset by the range's first vertex. `false` when it does not
    /// fit.
    pub(super) fn write(&mut self, gpu: &Gpu, r: &ArenaRange, data: &MeshData) -> bool {
        if data.vertices.len() as u64 > r.v_cap as u64 || data.indices.len() as u64 > r.i_cap as u64 {
            return false;
        }
        // An index past the mesh's own vertices would draw a neighbour's in
        // the page (own buffers would read stale bytes of the same mesh).
        let nv = data.vertices.len() as u32;
        if let Some(bad) = data.indices.iter().find(|&&i| i >= nv) {
            log::error!("a mesh of {nv} vertices has index {bad}: not drawn");
            return false;
        }
        let (vbuf, _) = self.buffers(r.page);
        if !data.vertices.is_empty() {
            gpu.queue.write_buffer(vbuf, r.v_off as u64 * VERTEX_BYTES, bytemuck::cast_slice(&data.vertices));
        }
        self.written += std::mem::size_of_val(data.vertices.as_slice()) as u64;
        self.write_indices(gpu, r, &data.indices);
        true
    }

    /// Write a slot's indices (counted from its own first vertex) into its
    /// range, offset by the range's first vertex.
    pub(super) fn write_indices(&mut self, gpu: &Gpu, r: &ArenaRange, indices: &[u32]) {
        if indices.is_empty() {
            return;
        }
        let moved: Vec<u32> = indices.iter().map(|&i| i + r.v_off).collect();
        let (_, ibuf) = self.buffers(r.page);
        gpu.queue.write_buffer(ibuf, r.i_off as u64 * 4, bytemuck::cast_slice(&moved));
        self.written += indices.len() as u64 * 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freed_blocks_merge_back_into_the_whole() {
        let mut b = Buddy::new(1 << 12);
        let a = b.alloc(1024).unwrap();
        let c = b.alloc(256).unwrap();
        let d = b.alloc(2048).unwrap();
        assert_eq!(b.used(), 1024 + 256 + 2048);
        assert!(b.alloc(2048).is_none(), "only 768 left in blocks of at most 512");
        b.free(c, 256);
        b.free(a, 1024);
        b.free(d, 2048);
        assert_eq!(b.used(), 0);
        assert_eq!(b.alloc(1 << 12), Some(0), "everything freed is one block again");
    }

    #[test]
    fn a_grown_range_merges_its_new_half() {
        let mut b = Buddy::new(1024);
        let a = b.alloc(1024).unwrap();
        assert!(b.alloc(4).is_none());
        b.grow();
        assert_eq!(b.alloc(1024), Some(1024));
        b.free(1024, 1024);
        b.free(a, 1024);
        assert_eq!(b.alloc(2048), Some(0), "the grown range is whole once all of it is free");
    }

    #[test]
    fn a_dry_run_takes_and_grows_as_the_allocator_does() {
        // Compaction picks a page to empty by playing the moves on free
        // block counts first: a dry run that took a block the allocator
        // would not, or grew differently, would choose pages whose slots
        // then do not fit, or miss pages whose slots would.
        // An empty range grown is one free block, as the allocator has it.
        let mut empty = Buddy::new(1 << 10);
        let mut sim = SimBuddy::of(&empty);
        empty.grow();
        sim.grow();
        assert_eq!(empty.free_counts(), sim.counts, "an empty range grown is not one free block in the dry run");
        let mut b = Buddy::new(1 << 12);
        let mut seed = 11u64;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as u32) % n
        };
        let mut live: Vec<(u32, u32)> = Vec::new();
        for _ in 0..200 {
            let n = 4u32 << rnd(8);
            if rnd(3) == 0 {
                if let Some(at) = live.iter().position(|_| rnd(2) == 0) {
                    let (o, m) = live.swap_remove(at);
                    b.free(o, m);
                }
            } else if let Some(o) = b.alloc(n) {
                live.push((o, n));
            }
        }
        for step in 0..400 {
            let mut sim = SimBuddy::of(&b);
            let n = 4u32 << rnd(9);
            if rnd(8) == 0 && b.size() < 1 << 16 {
                sim.grow();
                b.grow();
            } else {
                let took = take_count(&mut sim.counts, n).is_some();
                match b.alloc(n) {
                    Some(o) => live.push((o, n)),
                    None => assert!(!took, "step {step}: the dry run took {n} where the allocator found no room"),
                }
            }
            assert_eq!(b.free_counts(), sim.counts, "step {step}: the dry run's free blocks differ from the allocator's");
            assert_eq!(b.size(), sim.size);
        }
    }

    #[test]
    fn churn_keeps_what_is_held_close_to_what_is_live() {
        // Thousands of slots of every size come and go, as chunks do on a
        // walk; what stays allocated is exactly what is live (no block is
        // lost), and a range never overlaps another.
        let mut b = Buddy::new(1 << 22);
        let mut live: Vec<(u32, u32)> = Vec::new();
        let mut seed = 7u64;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as u32) % n
        };
        for step in 0..20_000 {
            if live.len() < 300 && (rnd(3) != 0 || live.is_empty()) {
                let n = 1024u32 << rnd(5);
                if let Some(off) = b.alloc(n) {
                    assert!(
                        live.iter().all(|&(o, m)| off + n <= o || o + m <= off),
                        "step {step}: [{off}, {}) overlaps a live range",
                        off + n
                    );
                    live.push((off, n));
                }
            } else {
                let (o, n) = live.swap_remove(rnd(live.len() as u32) as usize);
                b.free(o, n);
            }
            assert_eq!(b.used(), live.iter().map(|&(_, n)| n).sum::<u32>());
        }
        for (o, n) in live.drain(..) {
            b.free(o, n);
        }
        assert_eq!(b.alloc(1 << 22), Some(0));
    }
}
