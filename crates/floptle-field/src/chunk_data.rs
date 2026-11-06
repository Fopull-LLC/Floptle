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
const _: () = assert!(BLOCKS == 64, "a chunk's paged blocks are one u64 mask");

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
    /// The fine paint: palette slots finer than a voxel, in the blocks a
    /// brush painted finer than one. Boxed, so a chunk with none holds one
    /// empty pointer and nothing on the heap.
    fine: Option<Box<Fine>>,
}

/// A chunk's sub-voxel slots ([`ChunkData::set_fine_slot`]): per block,
/// either nothing (read the voxels' slots) or a page of `(8·factor)³` slots,
/// x fastest, the slot as the voxel colour's alpha holds it.
///
/// A page overrides its block's voxel slots wherever it is read, so it is
/// made from them (each sub-voxel the slot of the nearest voxel in the block)
/// and then written, and a page that holds nothing but that again is
/// dropped when the chunk is packed down.
#[derive(Clone)]
struct Fine {
    /// Sub-voxels per voxel per axis: 2 or 4.
    factor: u8,
    /// Per block: its page in `pages`, or [`FILL`].
    slot: [u16; BLOCKS],
    pages: Vec<Box<[u8]>>,
}

impl Fine {
    fn edge(&self) -> usize {
        B * self.factor as usize
    }
}

/// The voxel of a block nearest sub-voxel `j` of it (per axis), at `factor`
/// sub-voxels a voxel: `round(j / factor)`, halves up, and kept inside the
/// block, so a page is made from its own block alone.
#[inline]
fn nearest_voxel(j: usize, factor: usize) -> usize {
    ((2 * j + factor) / (2 * factor)).min(B - 1)
}

/// The block holding chunk-local sub-voxel `sub` at `factor`.
pub(crate) fn fine_block(sub: [usize; 3], factor: u8) -> usize {
    split_fine(sub, factor as usize).0
}

/// The block holding chunk-local sub-voxel `sub` at `factor`, and the
/// sub-voxel's index in that block's page.
#[inline]
fn split_fine(sub: [usize; 3], factor: usize) -> (usize, usize) {
    let n = B * factor;
    let b = ((sub[2] / n) * BE + sub[1] / n) * BE + sub[0] / n;
    (b, ((sub[2] % n) * n + sub[1] % n) * n + sub[0] % n)
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
            fine: None,
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

    /// Whether every voxel in the inclusive chunk-local range `lo ..= hi`
    /// holds a distance in `min ..= max`. A block holding a single value
    /// answers by it; one holding a page of values is read, but only where
    /// the range covers it.
    pub(crate) fn fills_within(&self, lo: [usize; 3], hi: [usize; 3], min: f32, max: f32) -> bool {
        let within = |v: f32| v >= min && v <= max;
        for bz in lo[2] / B..=hi[2] / B {
            for by in lo[1] / B..=hi[1] / B {
                for bx in lo[0] / B..=hi[0] / B {
                    let b = (bz * BE + by) * BE + bx;
                    let s = self.dist_slot[b];
                    if s == FILL {
                        if !within(self.dist_fill[b]) {
                            return false;
                        }
                        continue;
                    }
                    let page = &self.dist_pages[s as usize];
                    let span = |k: usize, base: usize| (lo[k].max(base) - base, hi[k].min(base + B - 1) - base);
                    let ((x0, x1), (y0, y1), (z0, z1)) = (span(0, bx * B), span(1, by * B), span(2, bz * B));
                    for z in z0..=z1 {
                        for y in y0..=y1 {
                            for x in x0..=x1 {
                                if !within(page[(z * B + y) * B + x]) {
                                    return false;
                                }
                            }
                        }
                    }
                }
            }
        }
        true
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

    /// Set a voxel's colour. Where its block has a fine page and the slot
    /// changes, the page's sub-voxels nearest the voxel take the new slot
    /// too, so ground a sculpt lays down in a finely painted block shows
    /// the slot it was laid with rather than what the page held before.
    pub(crate) fn set_color(&mut self, li: usize, c: [u8; 4]) {
        let old = if self.fine.is_some() { Some(self.color(li)[3]) } else { None };
        self.set_color_voxel(li, c);
        if let Some(old) = old
            && old != c[3]
        {
            self.write_through(li, c[3]);
        }
    }

    /// The sub-voxels of voxel `li`'s block page that read it as their
    /// nearest voxel take `slot`. Nothing where the block has no page.
    fn write_through(&mut self, li: usize, slot: u8) {
        let Some(fine) = self.fine.as_deref_mut() else { return };
        let (b, _) = split(li);
        let p = fine.slot[b];
        if p == FILL {
            return;
        }
        let (f, n) = (fine.factor as usize, fine.edge());
        let v = [li % CHUNK_U % B, (li / CHUNK_U) % CHUNK_U % B, li / (CHUNK_U * CHUNK_U) % B];
        // The sub-voxels whose nearest voxel is `v`: from half a voxel
        // below it to half one above, the block's edges taking the rest.
        let span = |v: usize| ((v * f).saturating_sub(f / 2), if v == B - 1 { n } else { v * f + f / 2 });
        let ((x0, x1), (y0, y1), (z0, z1)) = (span(v[0]), span(v[1]), span(v[2]));
        let page = &mut fine.pages[p as usize];
        for z in z0..z1 {
            for y in y0..y1 {
                page[(z * n + y) * n + x0..(z * n + y) * n + x1].fill(slot);
            }
        }
    }

    /// [`Self::set_color`] without touching the fine paint.
    fn set_color_voxel(&mut self, li: usize, c: [u8; 4]) {
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

    // ---- fine paint ------------------------------------------------------------

    /// Sub-voxels per voxel per axis of this chunk's fine pages; 0 with none.
    pub(crate) fn fine_factor(&self) -> u8 {
        self.fine.as_ref().map_or(0, |f| f.factor)
    }

    /// One bit per block holding a fine page, block `b` at bit `b`.
    pub(crate) fn fine_mask(&self) -> u64 {
        let Some(fine) = &self.fine else { return 0 };
        (0..BLOCKS).filter(|b| fine.slot[*b] != FILL).fold(0, |m, b| m | 1 << b)
    }

    /// Which block voxel `li` is in.
    pub(crate) fn block_of(li: usize) -> usize {
        split(li).0
    }

    /// The fine slot at chunk-local sub-voxel `sub` (each axis in
    /// `0 .. 32·factor`), where its block has a page at `factor`.
    pub(crate) fn fine_slot(&self, sub: [usize; 3], factor: u8) -> Option<u8> {
        let fine = self.fine.as_deref().filter(|f| f.factor == factor)?;
        let (b, i) = split_fine(sub, factor as usize);
        match fine.slot[b] {
            FILL => None,
            p => Some(fine.pages[p as usize][i]),
        }
    }

    /// The chunk-local voxel a page made in sub-voxel `sub`'s block takes it
    /// from: the nearest one in that block.
    pub(crate) fn fine_nearest_li(sub: [usize; 3], factor: u8) -> usize {
        let f = factor as usize;
        let n = B * f;
        let v = |k: usize| (sub[k] / n) * B + nearest_voxel(sub[k] % n, f);
        (v(2) * CHUNK_U + v(1)) * CHUNK_U + v(0)
    }

    /// The slot sub-voxel `sub` reads: its page's where its block has one,
    /// else that of the voxel of its block nearest it, which is what a page
    /// made there would start with.
    pub(crate) fn fine_read(&self, sub: [usize; 3], factor: u8) -> u8 {
        match self.fine_slot(sub, factor) {
            Some(s) => s,
            None => self.color(Self::fine_nearest_li(sub, factor))[3],
        }
    }

    /// Block `b`'s voxel slots, upsampled nearest into a page at `factor`.
    fn upsample(&self, b: usize, factor: usize) -> Box<[u8]> {
        let (bx, by, bz) = (b % BE * B, b / BE % BE * B, b / (BE * BE) * B);
        let mut voxels = [0u8; BV];
        for (i, v) in voxels.iter_mut().enumerate() {
            let (x, y, z) = (i % B, i / B % B, i / (B * B));
            *v = self.color(((bz + z) * CHUNK_U + by + y) * CHUNK_U + bx + x)[3];
        }
        let n = B * factor;
        let mut page = vec![0u8; n * n * n].into_boxed_slice();
        for z in 0..n {
            for y in 0..n {
                let row = (nearest_voxel(z, factor) * B + nearest_voxel(y, factor)) * B;
                for x in 0..n {
                    page[(z * n + y) * n + x] = voxels[row + nearest_voxel(x, factor)];
                }
            }
        }
        page
    }

    /// Paint sub-voxel `sub` with `slot` at `factor`, making its block's
    /// page first if it has none. Pages at another factor are resampled to
    /// this one first ([`Self::resample_fine`]). Whether anything changed.
    pub(crate) fn set_fine_slot(&mut self, factor: u8, sub: [usize; 3], slot: u8) -> bool {
        if self.fine_factor() != factor {
            self.resample_fine(factor);
        }
        let f = factor as usize;
        let (b, i) = split_fine(sub, f);
        let has_page = self.fine.as_ref().is_some_and(|fine| fine.slot[b] != FILL);
        if !has_page {
            if self.fine_read(sub, factor) == slot {
                return false;
            }
            let page = self.upsample(b, f);
            let fine = self.fine.get_or_insert_with(|| Box::new(Fine { factor, slot: [FILL; BLOCKS], pages: Vec::new() }));
            fine.pages.push(page);
            fine.slot[b] = (fine.pages.len() - 1) as u16;
        }
        let fine = self.fine.as_deref_mut().expect("made above");
        let cell = &mut fine.pages[fine.slot[b] as usize][i];
        let changed = *cell != slot;
        *cell = slot;
        changed
    }

    /// Bring every fine page to `factor` sub-voxels a voxel, each new
    /// sub-voxel taking the slot of the old one nearest it. A coarser factor
    /// loses what was finer than it; going back does not bring it back.
    pub(crate) fn resample_fine(&mut self, factor: u8) {
        let Some(fine) = self.fine.as_deref_mut() else { return };
        if fine.factor == factor {
            return;
        }
        let (old_n, new_n) = (fine.edge(), B * factor as usize);
        // Sub-voxel j at j/new voxels; the old sub-voxel nearest it.
        let near = |j: usize| ((2 * j * fine.factor as usize + factor as usize) / (2 * factor as usize)).min(old_n - 1);
        for page in &mut fine.pages {
            let mut out = vec![0u8; new_n * new_n * new_n].into_boxed_slice();
            for z in 0..new_n {
                for y in 0..new_n {
                    let row = (near(z) * old_n + near(y)) * old_n;
                    for x in 0..new_n {
                        out[(z * new_n + y) * new_n + x] = page[row + near(x)];
                    }
                }
            }
            *page = out;
        }
        fine.factor = factor;
    }

    /// Block `b`'s fine page, if it has one.
    pub(crate) fn fine_page(&self, b: usize) -> Option<&[u8]> {
        let fine = self.fine.as_deref()?;
        match fine.slot[b] {
            FILL => None,
            p => Some(&fine.pages[p as usize]),
        }
    }

    /// Give block `b` this page at `factor` (what a file held), replacing
    /// any it had. Pages at another factor are resampled to this one first.
    pub(crate) fn set_fine_page(&mut self, factor: u8, b: usize, page: Box<[u8]>) {
        debug_assert_eq!(page.len(), (B * factor as usize).pow(3));
        if self.fine_factor() != factor {
            self.resample_fine(factor);
        }
        let fine = self.fine.get_or_insert_with(|| Box::new(Fine { factor, slot: [FILL; BLOCKS], pages: Vec::new() }));
        match fine.slot[b] {
            FILL => {
                fine.pages.push(page);
                fine.slot[b] = (fine.pages.len() - 1) as u16;
            }
            p => fine.pages[p as usize] = page,
        }
    }

    /// Bytes the fine paint holds: its table and its pages.
    pub(crate) fn fine_bytes(&self) -> usize {
        self.fine.as_deref().map_or(0, |f| {
            std::mem::size_of::<Fine>()
                + f.pages.capacity() * std::mem::size_of::<Box<[u8]>>()
                + f.pages.iter().map(|p| p.len()).sum::<usize>()
        })
    }

    /// Drop every fine page that holds just what its block's voxels say,
    /// and the table once no page is left: what a page reads then is what
    /// the voxels read.
    fn shrink_fine(&mut self) {
        let Some(fine) = self.fine.take() else { return };
        let f = fine.factor as usize;
        let mut kept = Fine { factor: fine.factor, slot: [FILL; BLOCKS], pages: Vec::new() };
        let mut pages: Vec<Option<Box<[u8]>>> = fine.pages.into_iter().map(Some).collect();
        for b in 0..BLOCKS {
            let p = fine.slot[b];
            if p == FILL {
                continue;
            }
            let page = pages[p as usize].take().expect("one block a page");
            if *page != *self.upsample(b, f) {
                kept.pages.push(page);
                kept.slot[b] = (kept.pages.len() - 1) as u16;
            }
        }
        if !kept.pages.is_empty() {
            kept.pages.shrink_to_fit();
            self.fine = Some(Box::new(kept));
        }
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
        // A fine page holds slots, the colours' alpha: what `f` makes of a
        // slot it makes of every sub-voxel holding it.
        if let Some(fine) = self.fine.as_deref_mut() {
            let map: [u8; 256] = std::array::from_fn(|s| f([0, 0, 0, s as u8])[3]);
            fine.pages.iter_mut().flat_map(|p| p.iter_mut()).for_each(|s| *s = map[*s as usize]);
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
        // Kept at exactly what it holds: pushed one at a time it grew by
        // doubling, and a stored chunk lives for the whole session.
        pages.shrink_to_fit();
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
                kept.shrink_to_fit();
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
                kept_palette.shrink_to_fit();
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
                kept.shrink_to_fit();
                *pages = kept;
            }
        }
        // Last, against the voxel slots as they now are.
        self.shrink_fine();
    }

    /// Bytes this chunk holds, its fixed tables and its fine paint included.
    pub(crate) fn heap_bytes(&self) -> usize {
        // Capacity, not length: a page vector that grew holds room for more,
        // and that room is memory as much as the pages are.
        let pages = match &self.colors {
            Colors::Palette { palette, pages, .. } => palette.capacity() * 4 + pages.capacity() * BV,
            Colors::Raw { pages, .. } => pages.capacity() * BV * 4 + BLOCKS * 4,
        };
        // The Arc that holds it: two counts beside the data.
        2 * std::mem::size_of::<usize>()
            + std::mem::size_of::<Self>()
            + self.dist_pages.capacity() * BV * 4
            + pages
            + self.fine_bytes()
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

    /// **A fine page follows the voxels written into its block, and goes
    /// once it says nothing they do not.** A voxel's new slot writes through
    /// to the sub-voxels nearest it, and only those; a page holding just
    /// what its voxels say is dropped when the chunk is packed, and the
    /// chunk then holds nothing of the fine paint at all.
    #[test]
    fn a_fine_page_follows_its_voxels_and_folds_away() {
        for factor in [2u8, 4] {
            let f = factor as usize;
            let n = B * f;
            let mut d = ChunkData::new(0.0, [9, 9, 9, 1]);
            let bare = d.heap_bytes();
            assert_eq!((d.fine_factor(), d.fine_mask(), d.fine_bytes()), (0, 0, 0));
            // One sub-voxel of block 0 painted: a page, all 1 but it.
            assert!(d.set_fine_slot(factor, [3, 2, 1], 5));
            assert!(!d.set_fine_slot(factor, [3, 2, 1], 5), "the same paint twice changes nothing");
            assert_eq!(d.fine_mask(), 1);
            assert_eq!(d.fine_page(0).map(|p| p.len()), Some(n * n * n));
            // Voxel (4, 4, 4) of block 0 takes slot 7: the sub-voxels whose
            // nearest voxel it is take it, from half a voxel below to half one above.
            d.set_color((4 * CHUNK_U + 4) * CHUNK_U + 4, [9, 9, 9, 7]);
            for z in 0..n {
                for y in 0..n {
                    for x in 0..n {
                        let near = |j: usize| nearest_voxel(j, f) == 4;
                        let want = if near(x) && near(y) && near(z) {
                            7
                        } else if [x, y, z] == [3, 2, 1] {
                            5
                        } else {
                            1
                        };
                        assert_eq!(d.fine_read([x, y, z], factor), want, "{factor}×: sub-voxel {x} {y} {z}");
                    }
                }
            }
            // Painting over the one sub-voxel and packing: nothing the voxels
            // do not say, so no page and no table.
            d.set_fine_slot(factor, [3, 2, 1], 1);
            d.shrink();
            assert_eq!((d.fine_mask(), d.fine_bytes()), (0, 0));
            d.set_color((4 * CHUNK_U + 4) * CHUNK_U + 4, [9, 9, 9, 1]);
            d.shrink();
            assert_eq!(d.heap_bytes(), bare, "{factor}×: back to what it cost before any fine paint");
        }
    }
}
