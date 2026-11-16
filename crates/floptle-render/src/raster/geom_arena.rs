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
use std::collections::BTreeSet;

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
}

const VERTEX_BYTES: u64 = std::mem::size_of::<Vertex>() as u64;

/// A new page starts this large (512 KiB of vertices, 128 KiB of indices),
/// so a scene with a sprite or two does not hold a planet's worth.
const FIRST_PAGE_VERTS: u32 = 1 << 14;
const FIRST_PAGE_INDICES: u32 = 1 << 15;

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
        let fit = |per: u64, cap: u32| -> u32 {
            let n = (limit / per).min(cap as u64) as u32;
            if n.is_power_of_two() { n } else { n.next_power_of_two() / 2 }
        };
        GeomArena { pages: Vec::new(), max_verts: fit(VERTEX_BYTES, 1 << 21), max_indices: fit(4, 1 << 23), written: 0 }
    }

    /// Bytes the pages hold on the GPU.
    pub(super) fn bytes(&self) -> u64 {
        self.pages.iter().flatten().map(|p| p.vbuf.size() + p.ibuf.size()).sum()
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
    pub(super) fn alloc(&mut self, gpu: &Gpu, v_cap: u32, i_cap: u32) -> (Option<ArenaRange>, Option<u32>) {
        if v_cap > self.max_verts || i_cap > self.max_indices {
            return (None, None);
        }
        for (i, p) in self.pages.iter_mut().enumerate() {
            let Some(p) = p else { continue };
            if let Some(r) = Self::take(p, i as u32, v_cap, i_cap) {
                return (Some(r), None);
            }
        }
        // Grow the first page that could hold it once grown. Free space is
        // not always one block, so it grows until the ranges are there or
        // it is as large as a page gets.
        let fits = |b: &Buddy, want: u32, max: u32| {
            let mut s = b.size();
            while s - b.used() < want && s < max {
                s *= 2;
            }
            s - b.used() >= want
        };
        let grower = self.pages.iter().position(|p| {
            p.as_ref().is_some_and(|p| fits(&p.verts, v_cap, self.max_verts) && fits(&p.indices, i_cap, self.max_indices))
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
        let short = |b: &Buddy, want: u32| b.size() - b.used() < want || b.size() < want * 2;
        let grow_v = p.verts.size() < max_v && short(&p.verts, v_cap);
        let grow_i = p.indices.size() < max_i && short(&p.indices, i_cap);
        let (grow_v, grow_i) = if grow_v || grow_i {
            (grow_v, grow_i)
        } else {
            (p.verts.size() < max_v, p.indices.size() < max_i)
        };
        if !grow_v && !grow_i {
            return false;
        }
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
        let (vbuf, ibuf) = self.buffers(r.page);
        if !data.vertices.is_empty() {
            gpu.queue.write_buffer(vbuf, r.v_off as u64 * VERTEX_BYTES, bytemuck::cast_slice(&data.vertices));
        }
        if !data.indices.is_empty() {
            let moved: Vec<u32> = data.indices.iter().map(|&i| i + r.v_off).collect();
            gpu.queue.write_buffer(ibuf, r.i_off as u64 * 4, bytemuck::cast_slice(&moved));
        }
        self.written += std::mem::size_of_val(data.vertices.as_slice()) as u64 + data.indices.len() as u64 * 4;
        true
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
