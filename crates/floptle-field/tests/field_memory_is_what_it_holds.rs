//! **What `ChunkField::memory` reports is what a field holds.** A generated
//! planet's reported bytes are within a tenth of the heap it keeps, measured
//! by counting every allocation (its own binary, so the count is this test's
//! alone). `terrain.stats` is only worth reading if this holds.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size as isize - layout.size() as isize, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn a_generated_planets_reported_memory_is_what_it_holds() {
    let spec = floptle_field::procgen::PlanetFill { radius: 60.0, voxel: 1.0, seed: 7, ..Default::default() };
    let before = LIVE.load(Ordering::Relaxed);
    let field = floptle_field::procgen::generate_planet(&spec);
    let held = (LIVE.load(Ordering::Relaxed) - before) as f64;
    let reported = field.memory(&mut std::collections::HashSet::new());
    assert!(reported.data_chunks > 50, "fixture: only {} chunks", reported.data_chunks);
    let total = reported.total() as f64;
    let off = (total - held).abs() / held;
    assert!(
        off <= 0.10,
        "the field holds {:.1} MB and reports {:.1} MB ({:.0} % off): data {}, stand-ins {}, tables {}",
        held / 1e6,
        total / 1e6,
        off * 100.0,
        reported.data,
        reported.stand_ins,
        reported.tables
    );
    drop(field);
}
