//! The voxels of one stored chunk, kept in 8³ blocks.
//!
//! A chunk the surface passes through is mostly two plateaus, open air above
//! and saturated solid below, with the band between them; and its colours are
//! a few dozen tints of a few layers. Stored flat, every voxel cost eight
//! bytes whether it said anything or not, and a planet's field was most of a
//! gigabyte at 400 m. Here a block whose voxels all hold one distance keeps
//! that one value, colours are indices into the chunk's own palette, and a
//! block whose colours are all one keeps that one. Values are exact: what is
//! read back is what was written.
//!
//! Voxels are addressed as before, by their index in the chunk, x fastest.

use crate::chunks::{CHUNK_U, CHUNK_VOXELS};

/// Voxels per block edge, and per block.
const B: usize = 8;
const BV: usize = B * B * B;
/// Blocks per chunk edge, and per chunk.
const BE: usize = CHUNK_U / B;
const BLOCKS: usize = BE * BE * BE;
/// "No page: the block holds its fill value throughout."
const FILL: u16 = u16::MAX;

/// Which block a voxel is in, and where in it.
#[inline]
fn split(li: usize) -> (usize, usize) {
    let (x, y, z) = (li % CHUNK_U, (li / CHUNK_U) % CHUNK_U, li / (CHUNK_U * CHUNK_U));
    ((((z / B) * BE) + y / B) * BE + x / B, (((z % B) * B) + y % B) * B + x % B)
}

#[derive(Clone)]
pub(crate) struct ChunkData {
    /// Per block: its page in `dist_pages`, or [`FILL`].
    dist_slot: [u16; BLOCKS],
    dist_fill: [f32; BLOCKS],
    dist_pages: Vec<[f32; BV]>,
    /// Per block: its page in the colour pages, or [`FILL`].
    color_slot: [u16; BLOCKS],
    colors: Colors,
}

/// A chunk's colours: palette indices while it has 256 or fewer, and the
/// colours themselves once it has more (a painted or imported chunk can).
///
/// The palette's fill stays inline because every colour read goes through it;
/// there is one of these per stored chunk, which is boxed already.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum Colors {
    Palette { palette: Vec<[u8; 4]>, fill: [u8; BLOCKS], pages: Vec<[u8; BV]> },
    Raw { fill: Box<[[u8; 4]; BLOCKS]>, pages: Vec<[[u8; 4]; BV]> },
}

impl std::fmt::Debug for ChunkData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChunkData({} distance pages, {} bytes)", self.dist_pages.len(), self.heap_bytes())
    }
}

impl ChunkData {
    /// Every voxel at `dist`, coloured `color`.
    pub(crate) fn new(dist: f32, color: [u8; 4]) -> Self {
        Self {
            dist_slot: [FILL; BLOCKS],
            dist_fill: [dist; BLOCKS],
            dist_pages: Vec::new(),
            color_slot: [FILL; BLOCKS],
            colors: Colors::Palette { palette: vec![color], fill: [0; BLOCKS], pages: Vec::new() },
        }
    }

    /// From flat arrays, `CHUNK_VOXELS` long, x fastest.
    pub(crate) fn from_flat(dist: &[f32], color: &[[u8; 4]]) -> Self {
        let mut d = Self::new(dist[0], color[0]);
        for li in 0..CHUNK_VOXELS {
            d.set_dist(li, dist[li]);
            d.set_color(li, color[li]);
        }
        d.shrink();
        d
    }

    #[inline]
    pub(crate) fn dist(&self, li: usize) -> f32 {
        let (b, i) = split(li);
        match self.dist_slot[b] {
            FILL => self.dist_fill[b],
            s => self.dist_pages[s as usize][i],
        }
    }

    #[inline]
    pub(crate) fn color(&self, li: usize) -> [u8; 4] {
        let (b, i) = split(li);
        let slot = self.color_slot[b];
        match &self.colors {
            Colors::Palette { palette, fill, pages } => {
                palette[if slot == FILL { fill[b] } else { pages[slot as usize][i] } as usize]
            }
            Colors::Raw { fill, pages } => {
                if slot == FILL {
                    fill[b]
                } else {
                    pages[slot as usize][i]
                }
            }
        }
    }

    pub(crate) fn set_dist(&mut self, li: usize, v: f32) {
        let (b, i) = split(li);
        let s = match self.dist_slot[b] {
            FILL if self.dist_fill[b].to_bits() == v.to_bits() => return,
            FILL => {
                self.dist_pages.push([self.dist_fill[b]; BV]);
                let s = (self.dist_pages.len() - 1) as u16;
                self.dist_slot[b] = s;
                s
            }
            s => s,
        };
        self.dist_pages[s as usize][i] = v;
    }

    pub(crate) fn set_color(&mut self, li: usize, c: [u8; 4]) {
        let (b, i) = split(li);
        if let Colors::Palette { palette, .. } = &self.colors
            && palette.len() >= 256
            && !palette.contains(&c)
        {
            self.widen();
        }
        let slot = self.color_slot[b];
        match &mut self.colors {
            Colors::Palette { palette, fill, pages } => {
                let k = match palette.iter().position(|p| *p == c) {
                    Some(k) => k as u8,
                    None => {
                        palette.push(c);
                        (palette.len() - 1) as u8
                    }
                };
                if slot == FILL {
                    if fill[b] == k {
                        return;
                    }
                    pages.push([fill[b]; BV]);
                    self.color_slot[b] = (pages.len() - 1) as u16;
                    pages.last_mut().expect("just pushed")[i] = k;
                } else {
                    pages[slot as usize][i] = k;
                }
            }
            Colors::Raw { fill, pages } => {
                if slot == FILL {
                    if fill[b] == c {
                        return;
                    }
                    pages.push([fill[b]; BV]);
                    self.color_slot[b] = (pages.len() - 1) as u16;
                    pages.last_mut().expect("just pushed")[i] = c;
                } else {
                    pages[slot as usize][i] = c;
                }
            }
        }
    }

    /// From palette indices to the colours themselves, for a chunk that has
    /// run out of palette.
    fn widen(&mut self) {
        let Colors::Palette { palette, fill, pages } = &self.colors else { return };
        let fill = Box::new(std::array::from_fn(|b| palette[fill[b] as usize]));
        let pages = pages.iter().map(|p| std::array::from_fn(|i| palette[p[i] as usize])).collect();
        self.colors = Colors::Raw { fill, pages };
    }

    /// Copy the box of voxels `s ..= e` (chunk-local, per axis) into flat buffers
    /// `dim` wide, where `s` lands at `at`. A block holding `skip_dist` or
    /// `skip_color` throughout is not written: the buffers already hold those.
    ///
    /// Block by block, so each block's pages are found once, not once a voxel;
    /// the mesher gathers boxes of a million voxels at coarse strides, and most
    /// blocks in them are one value.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn copy_box(
        &self,
        s: [usize; 3],
        e: [usize; 3],
        dim: usize,
        at: usize,
        dist: &mut [f32],
        color: &mut [[u8; 4]],
        skip_dist: f32,
        skip_color: [u8; 4],
    ) {
        for bz in s[2] / B..=e[2] / B {
            for by in s[1] / B..=e[1] / B {
                for bx in s[0] / B..=e[0] / B {
                    let b = (bz * BE + by) * BE + bx;
                    let lo = [s[0].max(bx * B), s[1].max(by * B), s[2].max(bz * B)];
                    let hi = [e[0].min(bx * B + B - 1), e[1].min(by * B + B - 1), e[2].min(bz * B + B - 1)];
                    let n = hi[0] - lo[0] + 1;
                    let dslot = self.dist_slot[b];
                    let dfill = self.dist_fill[b];
                    let cslot = self.color_slot[b];
                    let cfill = match &self.colors {
                        Colors::Palette { palette, fill, .. } => palette[fill[b] as usize],
                        Colors::Raw { fill, .. } => fill[b],
                    };
                    let skip_d = dslot == FILL && dfill.to_bits() == skip_dist.to_bits();
                    let skip_c = cslot == FILL && cfill == skip_color;
                    if skip_d && skip_c {
                        continue;
                    }
                    for z in lo[2]..=hi[2] {
                        for y in lo[1]..=hi[1] {
                            let d0 = at + ((z - s[2]) * dim + (y - s[1])) * dim + (lo[0] - s[0]);
                            let i0 = ((z % B) * B + y % B) * B + lo[0] % B;
                            if !skip_d {
                                if dslot == FILL {
                                    dist[d0..d0 + n].fill(dfill);
                                } else {
                                    dist[d0..d0 + n].copy_from_slice(&self.dist_pages[dslot as usize][i0..i0 + n]);
                                }
                            }
                            if !skip_c {
                                if cslot == FILL {
                                    color[d0..d0 + n].fill(cfill);
                                } else {
                                    match &self.colors {
                                        Colors::Palette { palette, pages, .. } => {
                                            let page = &pages[cslot as usize];
                                            for k in 0..n {
                                                color[d0 + k] = palette[page[i0 + k] as usize];
                                            }
                                        }
                                        Colors::Raw { pages, .. } => {
                                            color[d0..d0 + n].copy_from_slice(&pages[cslot as usize][i0..i0 + n]);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The one distance every voxel holds, if there is one.
    pub(crate) fn uniform_dist(&self) -> Option<f32> {
        let first = self.dist(0);
        let same = |v: f32| v.to_bits() == first.to_bits();
        (0..BLOCKS).all(|b| match self.dist_slot[b] {
            FILL => same(self.dist_fill[b]),
            s => self.dist_pages[s as usize].iter().all(|v| same(*v)),
        })
        .then_some(first)
    }

    /// Rewrite every colour through `f` (a whole-field recolour).
    pub(crate) fn map_colors(&mut self, f: impl Fn([u8; 4]) -> [u8; 4]) {
        match &mut self.colors {
            Colors::Palette { palette, .. } => palette.iter_mut().for_each(|c| *c = f(*c)),
            Colors::Raw { fill, pages } => {
                fill.iter_mut().for_each(|c| *c = f(*c));
                pages.iter_mut().flatten().for_each(|c| *c = f(*c));
            }
        }
        self.shrink();
    }

    /// Fold every block whose page holds one value back into its fill, and
    /// drop the pages nothing points at.
    pub(crate) fn shrink(&mut self) {
        let mut pages = Vec::new();
        for b in 0..BLOCKS {
            let s = self.dist_slot[b];
            if s == FILL {
                continue;
            }
            let page = self.dist_pages[s as usize];
            if page.iter().all(|v| v.to_bits() == page[0].to_bits()) {
                self.dist_fill[b] = page[0];
                self.dist_slot[b] = FILL;
            } else {
                pages.push(page);
                self.dist_slot[b] = (pages.len() - 1) as u16;
            }
        }
        self.dist_pages = pages;

        let slots = &mut self.color_slot;
        match &mut self.colors {
            Colors::Palette { palette, fill, pages } => {
                // Equal colours under different indices (after a recolour) fold
                // together first, so a page of them is seen to be one colour.
                let canon: Vec<u8> =
                    (0..palette.len()).map(|k| palette.iter().position(|p| *p == palette[k]).unwrap_or(k) as u8).collect();
                let mut kept = Vec::new();
                for b in 0..BLOCKS {
                    fill[b] = canon[fill[b] as usize];
                    let s = slots[b];
                    if s == FILL {
                        continue;
                    }
                    let page = pages[s as usize].map(|k| canon[k as usize]);
                    if page.iter().all(|k| *k == page[0]) {
                        fill[b] = page[0];
                        slots[b] = FILL;
                    } else {
                        kept.push(page);
                        slots[b] = (kept.len() - 1) as u16;
                    }
                }
                *pages = kept;
                // And the palette keeps only what is still used, so a chunk
                // repainted many times does not run out of it.
                let mut used = [false; 256];
                for b in 0..BLOCKS {
                    used[fill[b] as usize] = true;
                }
                for page in pages.iter() {
                    for k in page {
                        used[*k as usize] = true;
                    }
                }
                let mut remap = [0u8; 256];
                let mut kept_palette = Vec::new();
                for (k, c) in palette.iter().enumerate() {
                    if used[k] {
                        remap[k] = kept_palette.len() as u8;
                        kept_palette.push(*c);
                    }
                }
                *palette = kept_palette;
                fill.iter_mut().for_each(|k| *k = remap[*k as usize]);
                pages.iter_mut().flatten().for_each(|k| *k = remap[*k as usize]);
            }
            Colors::Raw { fill, pages } => {
                let mut kept = Vec::new();
                for b in 0..BLOCKS {
                    let s = slots[b];
                    if s == FILL {
                        continue;
                    }
                    let page = pages[s as usize];
                    if page.iter().all(|c| *c == page[0]) {
                        fill[b] = page[0];
                        slots[b] = FILL;
                    } else {
                        kept.push(page);
                        slots[b] = (kept.len() - 1) as u16;
                    }
                }
                *pages = kept;
            }
        }
    }

    /// Bytes this chunk holds, its fixed tables included.
    pub(crate) fn heap_bytes(&self) -> usize {
        let pages = match &self.colors {
            Colors::Palette { palette, pages, .. } => palette.len() * 4 + pages.len() * BV,
            Colors::Raw { pages, .. } => pages.len() * BV * 4 + BLOCKS * 3,
        };
        std::mem::size_of::<Self>() + self.dist_pages.len() * BV * 4 + pages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever is written reads back exactly, in any order, through every
    /// way the chunk is read — one voxel, a box, the uniform check — and
    /// still does after the chunk is packed down; including a chunk with more
    /// colours than a palette holds.
    #[test]
    fn a_chunk_reads_back_what_was_written() {
        let mut flat_d = vec![6.0f32; CHUNK_VOXELS];
        let mut flat_c = vec![[1, 2, 3, 255]; CHUNK_VOXELS];
        let mut d = ChunkData::new(6.0, [1, 2, 3, 255]);
        let mut x = 0x1234_5678u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for round in 0..3 {
            for _ in 0..20000 {
                let li = next() as usize % CHUNK_VOXELS;
                let v = (next() % 97) as f32 * 0.125 - 6.0;
                // Round 2 has a few hundred colours, past what a palette holds.
                let c = [(next() % if round == 2 { 900 } else { 7 }) as u8, 9, (next() % 3) as u8, 255];
                d.set_dist(li, v);
                d.set_color(li, c);
                flat_d[li] = v;
                flat_c[li] = c;
            }
            for packed in [false, true] {
                if packed {
                    d.shrink();
                }
                for li in 0..CHUNK_VOXELS {
                    assert_eq!(d.dist(li).to_bits(), flat_d[li].to_bits(), "dist {li}");
                    assert_eq!(d.color(li), flat_c[li], "color {li}");
                }
                // A box straddling blocks, into buffers that skip nothing.
                let (s0, e0) = ([3usize, 5, 7], [19usize, 26, 30]);
                let dim = 40;
                let (mut bd, mut bc) = (vec![f32::NAN; dim * dim * dim], vec![[7u8; 4]; dim * dim * dim]);
                d.copy_box(s0, e0, dim, 0, &mut bd, &mut bc, f32::NAN, [7, 7, 7, 7]);
                for z in s0[2]..=e0[2] {
                    for y in s0[1]..=e0[1] {
                        for x in s0[0]..=e0[0] {
                            let li = (z * CHUNK_U + y) * CHUNK_U + x;
                            let di = ((z - s0[2]) * dim + (y - s0[1])) * dim + (x - s0[0]);
                            assert_eq!(bd[di].to_bits(), flat_d[li].to_bits(), "box dist {x} {y} {z}");
                            assert_eq!(bc[di], flat_c[li], "box color {x} {y} {z}");
                        }
                    }
                }
                assert_eq!(d.uniform_dist(), None);
            }
        }
        let u = ChunkData::from_flat(&vec![-6.0; CHUNK_VOXELS], &vec![[4, 4, 4, 4]; CHUNK_VOXELS]);
        assert_eq!(u.uniform_dist(), Some(-6.0));
        assert!(u.heap_bytes() < 8 * 1024, "a uniform chunk costs its tables, not its voxels: {}", u.heap_bytes());
    }
}
