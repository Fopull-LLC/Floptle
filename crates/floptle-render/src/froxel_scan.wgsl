// The froxel fog grid summed front to back (`Raymarch::march_fog`). Each
// cell holds what its slab of air scatters toward the camera and the share
// of light behind it the slab lets through. Air in front of air: the near
// one's light, plus the far one's dimmed by the near one, and the two
// shares multiplied. That join is associative, so `log2(slices)` passes,
// each joining every cell with the one `step` slices in front of it
// (Hillis and Steele's scan), leave each cell holding everything from the
// camera to its far side.

@group(0) @binding(0) var src: texture_2d<u32>;
// x = step (slices), y = rows in a slice.
@group(0) @binding(1) var<uniform> scan: vec4<u32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

fn cell(t: vec4<u32>) -> vec4<f32> {
    return vec4<f32>(bitcast<f32>(t.x), bitcast<f32>(t.y), bitcast<f32>(t.z), bitcast<f32>(t.w));
}

@fragment
fn fs(@builtin(position) clip: vec4<f32>) -> @location(0) vec4<u32> {
    let c = vec2<u32>(clip.xy);
    let here = textureLoad(src, c, 0);
    let back = scan.x * scan.y;
    if (c.y < back) {
        return here;
    }
    let near = cell(textureLoad(src, vec2<u32>(c.x, c.y - back), 0));
    let far = cell(here);
    let sc = near.rgb + near.w * far.rgb;
    let t = near.w * far.w;
    return vec4<u32>(bitcast<u32>(sc.r), bitcast<u32>(sc.g), bitcast<u32>(sc.b), bitcast<u32>(t));
}
