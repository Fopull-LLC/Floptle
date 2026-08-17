//! **STL** — what CAD and 3D-printing tools export.
//!
//! The simplest format here and the one with the least in it: triangles and
//! face normals, no UVs, no materials, no names, no units. Both the ASCII and
//! the binary flavours; `stl_io` tells them apart by content rather than by
//! extension, which matters because plenty of `.stl` files are mislabelled.
//!
//! **STL has no units and no up axis.** There is nothing to convert *to*
//! metres, because the file does not say what it is in — an STL is in whatever
//! the exporter had. It is passed through unscaled and the report says so, which
//! is the only honest thing to do: guessing millimetres because most printers
//! use them would silently shrink every model that was not.

use std::path::Path;

use crate::common::{Scene, SubMesh};
use crate::ConvertError;

pub fn read(src: &Path) -> Result<Scene, ConvertError> {
    let mut file = std::fs::File::open(src).map_err(|e| ConvertError::Io(e.to_string()))?;
    let stl = stl_io::read_stl(&mut file).map_err(|e| ConvertError::Malformed(e.to_string()))?;

    let mut sm = SubMesh {
        name: src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mesh".into()),
        base_color: [0.8, 0.8, 0.8, 1.0],
        ..Default::default()
    };

    for v in &stl.vertices {
        sm.positions.push([v[0], v[1], v[2]]);
    }
    for f in &stl.faces {
        sm.indices.push(f.vertices[0] as u32);
        sm.indices.push(f.vertices[1] as u32);
        sm.indices.push(f.vertices[2] as u32);
    }

    // **The file's own normals are deliberately not used.** STL stores one
    // normal per FACE, and this vertex list is shared between faces — so there
    // is no per-vertex normal to write, and half of real STL files have zeroed
    // or wrong face normals anyway. `ensure_normals` computes them from the
    // geometry, which is both correct and what every STL viewer already does.
    let mut out = Scene { meshes: vec![sm], ..Default::default() };
    out.report.materials = 0;
    out.report
        .warnings
        .push("STL carries no units, so the model is converted at its original scale.".into());
    out.report.dropped.push("STL has no materials, textures or UVs to carry".into());
    Ok(out)
}
