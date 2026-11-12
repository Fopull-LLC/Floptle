//! Fine paint on the GPU: the pages of texture slots a terrain paints finer
//! than its voxels (`floptle_field::ChunkField::set_fine_paint`), in one
//! storage buffer the meshed terrain reads (group 0, binding 15, fragment).
//!
//! A terrain chunk instance carries which fine volume it reads, so a pixel
//! goes straight to its own volume's table: no search over volumes, and a
//! terrain with no fine paint pays one test.
//!
//! The buffer is words (`u32`):
//!
//! ```text
//! header: VOLUMES × VOL_WORDS
//!   [0..3]  i32  the chunk at the low corner of the volume's chunk table
//!   [3]     u32  sub-voxels per voxel (0 = the volume is off)
//!   [4..7]  u32  the chunk table's size in chunks, per axis
//!   [7]     u32  where the chunk table starts
//!   [8]     f32  sub-voxels per field unit (factor / voxel)
//! chunk table: x fastest, a chunk's block table or NONE
//! block table: 64 words, a block's page or NONE
//! page: (8·factor)³ slots, four to a word, x fastest
//! ```
//!
//! The layout is mirrored by the `fine_*` functions in `raster.wgsl`; change
//! the two together (a test holds the constants).
//!
//! [`FineMirror`] is the CPU copy and the allocator. It knows nothing of
//! wgpu, so the bookkeeping is tested without a GPU; the raster owns the
//! buffer and uploads what the mirror marks dirty.

use std::collections::HashMap;

/// Volumes the header holds: one per terrain that paints fine.
pub const VOLUMES: usize = 32;
/// Words per volume in the header.
pub const VOL_WORDS: usize = 12;
/// "Nothing here": a chunk with no block table, a block with no page.
pub const NONE: u32 = u32::MAX;
/// Chunk edge in voxels and blocks per chunk edge (`floptle_field`).
const CHUNK: i32 = 32;
const BLOCK: i32 = 8;
const BLOCKS: usize = 64;
/// The buffer starts at one granule (the header fits) and grows in steps of
/// this many words (4 MiB)...
const GROW_WORDS: usize = 1 << 20;
/// ...up to this many (64 MiB). Past it a page is refused and its block
/// draws from the voxels.
pub const MAX_WORDS: usize = 16 << 20;
/// A chunk table never covers more chunks than this.
const MAX_TABLE: u64 = 1 << 20;
/// Dirty spans are tracked in granules of this many words.
const GRANULE: usize = 1024;

/// Words a page at `factor` takes.
pub const fn page_words(factor: u8) -> usize {
    let n = (BLOCK as usize) * factor as usize;
    (n * n * n).div_ceil(4)
}

struct Volume {
    factor: u8,
    scale: f32,
    lo: [i32; 3],
    dims: [u32; 3],
    table: u32,
    /// Each chunk's block table.
    chunks: HashMap<[i32; 3], u32>,
    /// Each block's page.
    pages: HashMap<([i32; 3], u8), u32>,
}

/// The CPU copy of the fine paint buffer, and its allocator.
pub struct FineMirror {
    words: Vec<u32>,
    /// Allocations past the header end here.
    top: usize,
    /// Freed spans by size, for reuse by the next of the same size.
    free: HashMap<usize, Vec<u32>>,
    vols: Vec<Option<Volume>>,
    dirty: Vec<bool>,
    /// The buffer must be made again at `words.len()` (it grew).
    grown: bool,
    /// Pages refused because the buffer was full, since the last ask.
    refused: usize,
}

impl Default for FineMirror {
    fn default() -> Self {
        Self::new()
    }
}

impl FineMirror {
    pub fn new() -> Self {
        let mut m = Self {
            words: vec![0; GRANULE],
            top: VOLUMES * VOL_WORDS,
            free: HashMap::new(),
            vols: (0..VOLUMES).map(|_| None).collect(),
            dirty: vec![false; 1],
            grown: true,
            refused: 0,
        };
        m.mark(0, VOLUMES * VOL_WORDS);
        m
    }

    /// The whole copy, as the buffer must hold it.
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Words in use, header included: what the pages and tables cost.
    pub fn used_words(&self) -> usize {
        self.top - self.free.iter().map(|(n, v)| n * v.len()).sum::<usize>()
    }

    /// Pages refused for want of room since the last call.
    pub fn take_refused(&mut self) -> usize {
        std::mem::take(&mut self.refused)
    }

    /// Whether volume `k` is set, and at which factor and voxel.
    pub fn volume(&self, k: usize) -> Option<(u8, f32)> {
        self.vols.get(k)?.as_ref().map(|v| (v.factor, v.factor as f32 / v.scale))
    }

    /// Make volume `k` read pages at `factor` on voxels of `voxel` field
    /// units, empty. A volume already so stays as it is; one at another
    /// factor or voxel loses its pages.
    pub fn set_volume(&mut self, k: usize, factor: u8, voxel: f32) {
        if self.volume(k) == Some((factor, voxel)) {
            return;
        }
        self.clear_volume(k);
        let scale = factor as f32 / voxel;
        self.vols[k] = Some(Volume { factor, scale, lo: [0; 3], dims: [0; 3], table: NONE, chunks: HashMap::new(), pages: HashMap::new() });
        self.write_header(k);
    }

    /// Volume `k` reads nothing, and everything it held is freed.
    pub fn clear_volume(&mut self, k: usize) {
        let Some(v) = self.vols[k].take() else { return };
        for (_, p) in v.pages {
            self.release(p, page_words(v.factor));
        }
        for (_, t) in v.chunks {
            self.release(t, BLOCKS);
        }
        if v.table != NONE {
            self.release(v.table, v.dims.iter().product::<u32>() as usize);
        }
        let h = k * VOL_WORDS;
        self.words[h..h + VOL_WORDS].fill(0);
        self.mark(h, VOL_WORDS);
    }

    /// Give block `block` of chunk `c` in volume `k` this page (`(8·factor)³`
    /// slots, x fastest), or take its page away with `None`. Returns false
    /// when there was no room and the block draws from its voxels.
    pub fn set_page(&mut self, k: usize, c: [i32; 3], block: u8, page: Option<&[u8]>) -> bool {
        let Some(factor) = self.vols[k].as_ref().map(|v| v.factor) else { return false };
        let pw = page_words(factor);
        let Some(page) = page else {
            self.drop_page(k, c, block);
            return true;
        };
        debug_assert_eq!(page.len(), (BLOCK as usize * factor as usize).pow(3));
        let Some(bt) = self.block_table(k, c) else {
            self.refused += 1;
            return false;
        };
        let have = self.vols[k].as_ref().and_then(|v| v.pages.get(&(c, block)).copied());
        let at = match have {
            Some(at) => at,
            None => {
                let Some(at) = self.alloc(pw) else {
                    self.refused += 1;
                    return false;
                };
                self.vols[k].as_mut().unwrap().pages.insert((c, block), at);
                self.words[bt as usize + block as usize] = at;
                self.mark(bt as usize + block as usize, 1);
                at
            }
        };
        let at = at as usize;
        for (w, four) in self.words[at..at + pw].iter_mut().zip(page.chunks(4)) {
            let mut b = [0u8; 4];
            b[..four.len()].copy_from_slice(four);
            *w = u32::from_le_bytes(b);
        }
        self.mark(at, pw);
        true
    }

    fn drop_page(&mut self, k: usize, c: [i32; 3], block: u8) {
        let Some(v) = self.vols[k].as_mut() else { return };
        let Some(at) = v.pages.remove(&(c, block)) else { return };
        let pw = page_words(v.factor);
        let bt = v.chunks[&c] as usize;
        let empty = !v.pages.keys().any(|(cc, _)| *cc == c);
        self.release(at, pw);
        self.words[bt + block as usize] = NONE;
        self.mark(bt + block as usize, 1);
        if empty {
            let v = self.vols[k].as_mut().unwrap();
            v.chunks.remove(&c);
            let slot = Self::table_index(v, c);
            let table = v.table as usize;
            self.release(bt as u32, BLOCKS);
            if let Some(i) = slot {
                self.words[table + i] = NONE;
                self.mark(table + i, 1);
            }
        }
    }

    /// Chunk `c`'s block table in volume `k`, made (and the chunk table
    /// grown to hold it) if it has none.
    fn block_table(&mut self, k: usize, c: [i32; 3]) -> Option<u32> {
        if let Some(&t) = self.vols[k].as_ref()?.chunks.get(&c) {
            return Some(t);
        }
        if Self::table_index(self.vols[k].as_ref()?, c).is_none() {
            self.grow_table(k, c)?;
        }
        let t = self.alloc(BLOCKS)?;
        self.words[t as usize..t as usize + BLOCKS].fill(NONE);
        self.mark(t as usize, BLOCKS);
        let v = self.vols[k].as_mut().unwrap();
        v.chunks.insert(c, t);
        let i = Self::table_index(v, c).unwrap();
        let at = v.table as usize + i;
        self.words[at] = t;
        self.mark(at, 1);
        Some(t)
    }

    fn table_index(v: &Volume, c: [i32; 3]) -> Option<usize> {
        let mut l = [0usize; 3];
        for a in 0..3 {
            let d = c[a] - v.lo[a];
            if d < 0 || d as u32 >= v.dims[a] {
                return None;
            }
            l[a] = d as usize;
        }
        Some((l[2] * v.dims[1] as usize + l[1]) * v.dims[0] as usize + l[0])
    }

    /// Grow volume `k`'s chunk table to cover `c`, with a chunk to spare on
    /// every side so a stroke along an edge does not grow it each chunk.
    fn grow_table(&mut self, k: usize, c: [i32; 3]) -> Option<()> {
        let v = self.vols[k].as_ref()?;
        let (mut lo, mut hi) = (c, c);
        if v.table != NONE {
            for a in 0..3 {
                lo[a] = lo[a].min(v.lo[a]);
                hi[a] = hi[a].max(v.lo[a] + v.dims[a] as i32 - 1);
            }
        }
        for a in 0..3 {
            lo[a] -= 1;
            hi[a] += 1;
        }
        let dims = [(hi[0] - lo[0] + 1) as u32, (hi[1] - lo[1] + 1) as u32, (hi[2] - lo[2] + 1) as u32];
        let n = dims.iter().map(|&d| d as u64).product::<u64>();
        if n > MAX_TABLE {
            return None;
        }
        let table = self.alloc(n as usize)?;
        let (t, len) = (table as usize, n as usize);
        self.words[t..t + len].fill(NONE);
        let v = self.vols[k].as_mut().unwrap();
        let (old, old_len) = (v.table, v.dims.iter().product::<u32>() as usize);
        v.lo = lo;
        v.dims = dims;
        v.table = table;
        let moved: Vec<(usize, u32)> = v.chunks.iter().map(|(cc, &bt)| (Self::table_index(v, *cc).unwrap(), bt)).collect();
        for (i, bt) in moved {
            self.words[t + i] = bt;
        }
        self.mark(t, len);
        if old != NONE {
            self.release(old, old_len);
        }
        self.write_header(k);
        Some(())
    }

    fn write_header(&mut self, k: usize) {
        let h = k * VOL_WORDS;
        let w = &mut self.words[h..h + VOL_WORDS];
        w.fill(0);
        if let Some(v) = &self.vols[k] {
            w[0] = v.lo[0] as u32;
            w[1] = v.lo[1] as u32;
            w[2] = v.lo[2] as u32;
            w[3] = v.factor as u32;
            w[4] = v.dims[0];
            w[5] = v.dims[1];
            w[6] = v.dims[2];
            w[7] = v.table;
            w[8] = v.scale.to_bits();
        }
        self.mark(h, VOL_WORDS);
    }

    fn alloc(&mut self, n: usize) -> Option<u32> {
        if let Some(at) = self.free.get_mut(&n).and_then(Vec::pop) {
            return Some(at);
        }
        if self.top + n > MAX_WORDS {
            return None;
        }
        let at = self.top;
        self.top += n;
        if self.top > self.words.len() {
            let len = self.top.next_multiple_of(GROW_WORDS).min(MAX_WORDS);
            self.words.resize(len, 0);
            self.dirty.resize(len / GRANULE, false);
            self.grown = true;
        }
        Some(at as u32)
    }

    fn release(&mut self, at: u32, n: usize) {
        self.free.entry(n).or_default().push(at);
    }

    fn mark(&mut self, at: usize, n: usize) {
        self.dirty[at / GRANULE..(at + n).div_ceil(GRANULE)].fill(true);
    }

    /// Whether the buffer must be made again at the mirror's size; asking
    /// clears it, and the whole copy is then dirty.
    pub fn take_grown(&mut self) -> bool {
        let g = std::mem::take(&mut self.grown);
        if g {
            self.dirty.fill(false);
        }
        g
    }

    /// The word spans changed since the last call, as `(first word, words)`,
    /// each run of changed granules once.
    pub fn take_dirty(&mut self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut g = 0;
        while g < self.dirty.len() {
            if !self.dirty[g] {
                g += 1;
                continue;
            }
            let start = g;
            while g < self.dirty.len() && self.dirty[g] {
                self.dirty[g] = false;
                g += 1;
            }
            out.push((start * GRANULE, (g - start) * GRANULE));
        }
        out
    }

    /// The slot a pixel at field point `p` of volume `k` reads at its
    /// nearest sub-voxel: the shader's `fine_locate` on the CPU, for tests.
    pub fn slot_at(&self, k: usize, p: [f32; 3]) -> Option<u8> {
        let h = k * VOL_WORDS;
        let w = &self.words;
        let f = w[h + 3] as i32;
        if f == 0 {
            return None;
        }
        let scale = f32::from_bits(w[h + 8]);
        let s = p.map(|x| (x * scale).round() as i32);
        let n = CHUNK * f;
        let bw = BLOCK * f;
        let c = s.map(|x| x.div_euclid(n));
        let sub = s.map(|x| x.rem_euclid(n));
        let lo = [w[h] as i32, w[h + 1] as i32, w[h + 2] as i32];
        let dims = [w[h + 4], w[h + 5], w[h + 6]];
        let mut l = [0u32; 3];
        for a in 0..3 {
            let d = c[a] - lo[a];
            if d < 0 || d as u32 >= dims[a] {
                return None;
            }
            l[a] = d as u32;
        }
        let bt = w[(w[h + 7] + (l[2] * dims[1] + l[1]) * dims[0] + l[0]) as usize];
        if bt == NONE {
            return None;
        }
        let b = sub.map(|x| x / bw);
        let page = w[bt as usize + ((b[2] * 4 + b[1]) * 4 + b[0]) as usize];
        if page == NONE {
            return None;
        }
        let q = sub.map(|x| x % bw);
        let i = ((q[2] * bw + q[1]) * bw + q[0]) as usize;
        Some((w[page as usize + i / 4] >> ((i % 4) * 8)) as u8)
    }
}

/// The layout entry for the buffer: storage, read-only, fragment.
pub const fn layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    crate::terrain_slots::slot_layout_entry(binding)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(factor: u8, f: impl Fn(usize, usize, usize) -> u8) -> Vec<u8> {
        let n = BLOCK as usize * factor as usize;
        let mut v = vec![0; n * n * n];
        for z in 0..n {
            for y in 0..n {
                for x in 0..n {
                    v[(z * n + y) * n + x] = f(x, y, z);
                }
            }
        }
        v
    }

    /// The shader's constants are these: the layout cannot drift on one
    /// side only.
    #[test]
    fn the_fine_buffer_header_matches_the_shader() {
        let src = include_str!("raster.wgsl");
        for (name, val) in [("FINE_VOL_WORDS", VOL_WORDS as u32), ("FINE_VOLUMES", VOLUMES as u32), ("FINE_NONE", NONE)] {
            let want = if val == NONE { format!("const {name}: u32 = 0xffffffffu;") } else { format!("const {name}: u32 = {val}u;") };
            assert!(src.contains(&want), "raster.wgsl must declare `{want}`");
        }
        // The fields the shader reads, at the words the mirror writes them.
        for (what, word) in [("fine_word(h + 3u)", 3), ("fine_word(h + 7u)", 7), ("fine_word(h + 8u)", 8)] {
            assert!(src.contains(what), "raster.wgsl reads header word {word} as `{what}`");
        }
    }

    #[test]
    fn a_page_reads_back_where_it_was_put_on_either_side_of_zero() {
        let mut m = FineMirror::new();
        m.set_volume(3, 2, 1.5);
        // Block 0 of chunk (-1, 0, 2), and block 63 (the far corner) of (0, 0, 0).
        let a = page(2, |x, y, z| (1 + (x + y * 3 + z * 5) % 7) as u8);
        assert!(m.set_page(3, [-1, 0, 2], 0, Some(&a)));
        assert!(m.set_page(3, [0, 0, 0], 63, Some(&page(2, |_, _, _| 9))));
        let sv = 1.5 / 2.0; // a sub-voxel in field units
        for (x, y, z) in [(0usize, 0usize, 0usize), (5, 2, 11), (15, 15, 15)] {
            let p = [(-64 + x as i32) as f32 * sv, y as f32 * sv, (128 + z as i32) as f32 * sv];
            assert_eq!(m.slot_at(3, p), Some(a[(z * 16 + y) * 16 + x]), "sub-voxel {x},{y},{z}");
        }
        assert_eq!(m.slot_at(3, [63.0 * sv, 63.0 * sv, 63.0 * sv]), Some(9));
        // A block of the same chunk with no page, and a chunk with none.
        assert_eq!(m.slot_at(3, [0.0, 0.0, 0.0]), None);
        assert_eq!(m.slot_at(3, [500.0, 0.0, 0.0]), None);
        // Another volume sees none of it.
        assert_eq!(m.slot_at(2, [63.0 * sv, 63.0 * sv, 63.0 * sv]), None);
    }

    #[test]
    fn freed_pages_are_reused_and_a_cleared_volume_costs_nothing() {
        let mut m = FineMirror::new();
        let base = m.used_words();
        m.set_volume(0, 4, 1.0);
        let p = page(4, |_, _, _| 2);
        for b in 0..10 {
            m.set_page(0, [0, 0, 0], b, Some(&p));
        }
        let ten = m.used_words();
        assert_eq!(ten - base, 10 * page_words(4) + BLOCKS + 27, "ten pages, a block table and a 3³ chunk table");
        for b in 0..10 {
            m.set_page(0, [0, 0, 0], b, None);
        }
        assert_eq!(m.used_words(), base + 27, "the block table goes with the chunk's last page");
        let top = m.top;
        m.set_page(0, [0, 0, 0], 5, Some(&p));
        assert_eq!(m.top, top, "a page freed is a page reused");
        m.clear_volume(0);
        assert_eq!(m.used_words(), base);
        assert_eq!(m.slot_at(0, [0.0, 0.0, 0.0]), None);
    }

    #[test]
    fn a_chunk_table_grows_to_hold_far_chunks_and_keeps_the_near_ones() {
        let mut m = FineMirror::new();
        m.set_volume(1, 2, 1.0);
        let p = page(2, |_, _, _| 4);
        m.set_page(1, [0, 0, 0], 0, Some(&p));
        m.set_page(1, [9, -7, 3], 0, Some(&page(2, |_, _, _| 6)));
        assert_eq!(m.slot_at(1, [0.0, 0.0, 0.0]), Some(4));
        assert_eq!(m.slot_at(1, [9.0 * 32.0, -7.0 * 32.0, 3.0 * 32.0]), Some(6));
    }

    #[test]
    fn a_full_buffer_refuses_the_page_and_says_so() {
        let mut m = FineMirror::new();
        m.set_volume(0, 4, 1.0);
        let p = page(4, |_, _, _| 1);
        let mut put = 0;
        for c in 0..400 {
            for b in 0..BLOCKS as u8 {
                if m.set_page(0, [c % 20, c / 20, 0], b, Some(&p)) {
                    put += 1;
                }
            }
        }
        assert!(m.words().len() <= MAX_WORDS);
        assert!(put < 400 * BLOCKS && put > 1000, "{put} pages fitted");
        assert_eq!(m.take_refused(), 400 * BLOCKS - put);
    }

    #[test]
    fn only_what_changed_is_dirty() {
        let mut m = FineMirror::new();
        m.take_grown();
        m.take_dirty();
        m.set_volume(0, 2, 1.0);
        m.set_page(0, [0, 0, 0], 0, Some(&page(2, |_, _, _| 1)));
        m.take_dirty();
        m.set_page(0, [0, 0, 0], 0, Some(&page(2, |_, _, _| 3)));
        let d = m.take_dirty();
        let words: usize = d.iter().map(|s| s.1).sum();
        assert!(words <= 2 * GRANULE, "a rewritten page uploads its own granules: {d:?}");
    }
}
