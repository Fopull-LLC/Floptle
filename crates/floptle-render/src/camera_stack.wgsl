// Camera stack: one stacked camera's picture laid over the frame beneath it.
//
// The layer was cleared to transparent black and depth 1.0, then drew only its
// own layers. A pixel it drew a solid surface into wrote depth there, and that
// is what decides coverage — not alpha, because an opaque material can write an
// alpha below one (a texture's own alpha passes the cutout at 0.5) and would
// otherwise come out see-through. Where nothing solid landed, whatever
// translucent surface did is already premultiplied against the clear, so the
// same blend lays it over the frame.

@group(0) @binding(0) var layer_color: texture_2d<f32>;
// Read as an unfilterable float texture, not texture_depth_2d: `textureLoad`
// on a depth texture does not exist on the OpenGL backend.
@group(0) @binding(1) var layer_depth: texture_2d<f32>;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = p[vi];
    var o: VOut;
    o.clip = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>(xy.x * 0.5 + 0.5, 1.0 - (xy.y * 0.5 + 0.5));
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    // By uv rather than by the target's pixel, so a layer at a different size
    // from the frame still lands over the whole of it.
    let dims = vec2<f32>(textureDimensions(layer_color));
    let px = vec2<i32>(clamp(in.uv * dims, vec2<f32>(0.0), dims - vec2<f32>(1.0)));
    let c = textureLoad(layer_color, px, 0);
    let solid = textureLoad(layer_depth, px, 0).x < 1.0;
    return select(c, vec4<f32>(c.rgb, 1.0), solid);
}
