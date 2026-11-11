//[group1-begin]
// The array build of the surface's six maps (`raster.rs` `array_source`,
// spliced over raster.wgsl's 2D block): each map is a texture array shared by
// pictures of one size and sampling (`texture_pool`), and each slot of a
// merged model reads its own layer of each, named in its slot-table entry and
// carried in `imeta.y/z` (see `layer_words`). Same bindings, same samplers,
// same kinds; only the dimension and the layer differ.
@group(1) @binding(0) var tex: texture_2d_array<f32>;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var normal_tex: texture_2d_array<f32>;
@group(1) @binding(3) var normal_samp: sampler;
@group(1) @binding(4) var rough_tex: texture_2d_array<f32>;
@group(1) @binding(5) var rough_samp: sampler;
@group(1) @binding(6) var metal_tex: texture_2d_array<f32>;
@group(1) @binding(7) var metal_samp: sampler;
@group(1) @binding(8) var ao_tex: texture_2d_array<f32>;
@group(1) @binding(9) var ao_samp: sampler;
@group(1) @binding(10) var emit_tex: texture_2d_array<f32>;
@group(1) @binding(11) var emit_samp: sampler;

// This invocation's six layers, set once at the top of each fragment entry.
var<private> g1_layer: array<i32, 6>;

fn g1_set_layers(m: vec4<u32>) {
    g1_layer = array<i32, 6>(
        i32(m.y & 255u), i32((m.y >> 8u) & 255u), i32((m.y >> 16u) & 255u), i32(m.y >> 24u),
        i32(m.z & 255u), i32((m.z >> 8u) & 255u),
    );
}

fn g1(k: u32, uv: vec2<f32>) -> vec4<f32> {
    switch k {
        case 0u: { return textureSample(tex, samp, uv, g1_layer[0]); }
        case 1u: { return textureSample(normal_tex, normal_samp, uv, g1_layer[1]); }
        case 2u: { return textureSample(rough_tex, rough_samp, uv, g1_layer[2]); }
        case 3u: { return textureSample(metal_tex, metal_samp, uv, g1_layer[3]); }
        case 4u: { return textureSample(ao_tex, ao_samp, uv, g1_layer[4]); }
        default: { return textureSample(emit_tex, emit_samp, uv, g1_layer[5]); }
    }
}

fn g1_grad(k: u32, uv: vec2<f32>, dx: vec2<f32>, dy: vec2<f32>) -> vec4<f32> {
    switch k {
        case 0u: { return textureSampleGrad(tex, samp, uv, g1_layer[0], dx, dy); }
        case 1u: { return textureSampleGrad(normal_tex, normal_samp, uv, g1_layer[1], dx, dy); }
        case 2u: { return textureSampleGrad(rough_tex, rough_samp, uv, g1_layer[2], dx, dy); }
        case 3u: { return textureSampleGrad(metal_tex, metal_samp, uv, g1_layer[3], dx, dy); }
        case 4u: { return textureSampleGrad(ao_tex, ao_samp, uv, g1_layer[4], dx, dy); }
        default: { return textureSampleGrad(emit_tex, emit_samp, uv, g1_layer[5], dx, dy); }
    }
}
//[group1-end]
