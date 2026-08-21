//! Print what a baked `.fnav` actually contains.
//!
//! For the question "the bake looks wrong, what did it find?" — which the
//! viewport can only answer in perspective, at a camera angle, with the rest of
//! the scene drawn over it.

use std::collections::BTreeMap;

/// What one region adds up to: polygons, area, its plan extent and height range.
#[derive(Clone, Copy)]
struct Tally {
    polys: usize,
    area: f32,
    lo: [f32; 2],
    hi: [f32; 2],
    y_min: f32,
    y_max: f32,
}

impl Default for Tally {
    fn default() -> Self {
        Self {
            polys: 0,
            area: 0.0,
            lo: [f32::MAX; 2],
            hi: [f32::MIN; 2],
            y_min: f32::MAX,
            y_max: f32::MIN,
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: nav_dump <file.fnav>");
    let bytes = std::fs::read(&path).expect("read");
    // A `.fnav` is `FNAV` + a u32 version + the postcard body. Reading the whole
    // file as a body parses the magic as data and fails with a varint error —
    // which is what this probe did to every real bake it was ever pointed at.
    let body = match bytes.strip_prefix(b"FNAV".as_slice()) {
        Some(rest) if rest.len() >= 4 => {
            let (v, body) = rest.split_at(4);
            let v = u32::from_le_bytes(v.try_into().expect("4 bytes"));
            println!("{}  format v{v}", path);
            body
        }
        // No header: a bake from before the format carried one. Say which,
        // rather than reporting it as a parse failure.
        _ => {
            eprintln!("{path}: not a versioned .fnav (no FNAV header) — rebake it");
            std::process::exit(1);
        }
    };
    let mesh: floptle_nav::NavMesh = postcard::from_bytes(body).expect("parse");

    println!("anchor {:?}  cell {}  settings {:?}", mesh.anchor, mesh.cell_size, mesh.settings);
    println!("{} polygons, {:.1} m² total", mesh.polys.len(), mesh.area());

    let mut by_region: BTreeMap<u32, Tally> = BTreeMap::new();
    for p in &mesh.polys {
        let e = by_region.entry(p.region).or_default();
        e.polys += 1;
        e.area += (p.max[0] - p.min[0]) * (p.max[1] - p.min[1]);
        for i in 0..2 {
            e.lo[i] = e.lo[i].min(p.min[i]);
            e.hi[i] = e.hi[i].max(p.max[i]);
        }
        e.y_min = e.y_min.min(p.y_min);
        e.y_max = e.y_max.max(p.y_max);
    }
    let mut rows: Vec<_> = by_region.into_iter().collect();
    rows.sort_by(|a, b| b.1.area.total_cmp(&a.1.area));
    println!("\n{:>6} {:>6} {:>9}  {:>28}  {:>16}", "region", "polys", "area m²", "x/z extent (local)", "y range");
    for (r, t) in rows {
        println!(
            "{r:>6} {:>6} {:>9.1}  x {:>6.1}..{:<6.1} z {:>6.1}..{:<6.1}  {:>7.2}..{:<7.2}",
            t.polys, t.area, t.lo[0], t.hi[0], t.lo[1], t.hi[1], t.y_min, t.y_max
        );
    }
}
