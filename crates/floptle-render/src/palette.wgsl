// The palette pass — posterize, and the one place in the renderer that quantizes.
//
// It runs over the frame the raster and raymarch passes have drawn and BEFORE
// the 2D light composite, which is the whole point of it existing as its own
// pass rather than as two lines at the end of `post.wgsl` (`floptle/0127`).
//
// ## Why the order is the feature
//
// A posterized project is quantizing its **palette**: the set of values its art
// is allowed to be. Light is not one of those values — it is a multiplier on
// whatever value the art is. Quantizing the two together makes them the same
// setting, and then there is no configuration that is right:
//
//   bands 8, dither off  → the light is concentric rings with hard edges
//   bands 8, dither on   → a stipple, which reads as a dither pattern, not light
//   bands off            → smooth, and the project loses the palette it chose
//
// Every row gives something up, so the correct behaviour was not one of the
// options a project could pick — it was missing. Quantizing here, before the
// light is added, is what makes all three rows right at once: the art steps, the
// light does not, and no scene has to be configured to get that.
//
// This is the same conflation `ambient_2d` was split out of the 3D `ambient` to
// fix: two different things confused because they landed in the same number.
//
// Everything downstream of the composite is therefore light-shaped by
// construction — the 2D delta, SSAO, bloom, the vignette — and none of it is
// quantized any more. That is not a side effect to apologise for; a vignette is
// a smooth radial darkening, and it was banding for exactly the same reason a
// light was.
//
// Both passes read with `textureLoad` rather than a sampler. The scratch target
// only GROWS (see `palette.rs`), so a frame smaller than the scratch occupies
// its top-left corner and a UV of 0..1 would read the wrong texels; integer
// coordinates are the same pixel in both textures whatever size the scratch has
// grown to. It also makes the round trip exact, which matters when the whole job
// is landing on a level and staying there.

struct P {
    // x = band count (< 2 = the pass is not run at all), y = ordered dither,
    // z = quantize brightness and carry the chroma, w spare.
    q: vec4<f32>,
    // xy = the frame's pixel size. Not the scratch's — that only grows.
    frame: vec4<f32>,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: P;

/// One triangle covering the target — no quad seam, same as every other
/// full-screen pass here.
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(corners[vi], 0.0, 1.0);
}

// Bayer 4×4 ordered-dither threshold in (0,1) (standalone copy — see field.wgsl).
fn bayer4(pix: vec2<u32>) -> f32 {
    var m = array<u32, 16>(0u, 8u, 2u, 10u, 12u, 4u, 14u, 6u, 3u, 11u, 1u, 9u, 15u, 7u, 13u, 5u);
    return (f32(m[(pix.y % 4u) * 4u + (pix.x % 4u)]) + 0.5) / 16.0;
}

/// Quantize `c` (linear) to `bands` levels, in ~gamma space so the steps are
/// perceptually even rather than crowded into the highlights.
fn quantize(c: vec3<f32>, px: vec2<i32>) -> vec3<f32> {
    let scale = p.q.x - 1.0;
    let g = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2)); // linear → ~gamma
    var t = 0.5;                                               // nearest level (= round)
    if (p.q.y > 0.5) {
        t = bayer4(vec2<u32>(u32(px.x), u32(px.y)));
    }
    // ---- brightness only, chroma carried along (`floptle/0126`) -------------
    //
    // Quantizing each channel on its own is a real look, and it is what a
    // *surface* usually wants — but a warm tint whose channels cross their band
    // boundaries at different values steps through hues nobody chose. Here the
    // step happens once, to luminance, and the pixel's own colour is scaled by
    // the ratio, so chroma is never quantized and that cannot happen.
    if (p.q.z > 0.5) {
        let y = dot(g, vec3<f32>(0.2126, 0.7152, 0.0722));
        let yq = clamp(floor(y * scale + t) / scale, 0.0, 1.0);
        // An exactly grey pixel takes the identical path it always did. Not an
        // optimization — it is the promise that switching this on cannot move
        // art that was already neutral, which is most of a 1-bit tileset.
        let mx = max(max(g.r, g.g), g.b);
        let mn = min(min(g.r, g.g), g.b);
        if (mx - mn < 1e-6) {
            return pow(vec3<f32>(yq), vec3<f32>(2.2));
        }
        let gq = clamp(g * (yq / max(y, 1e-5)), vec3<f32>(0.0), vec3<f32>(1.0));
        return pow(gq, vec3<f32>(2.2));
    }
    let gq = clamp(floor(g * scale + vec3<f32>(t)) / scale, vec3<f32>(0.0), vec3<f32>(1.0));
    return pow(gq, vec3<f32>(2.2)); // ~gamma → linear
}

/// Frame → scratch, quantized. Alpha is carried through untouched: this pass
/// runs over a target something else still has to composite or present, and a
/// full-screen write of `a = 1` would flatten whatever it was carrying.
@fragment
fn fs_quantize(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<i32>(pos.xy);
    let c = textureLoad(src, px, 0);
    return vec4<f32>(quantize(c.rgb, px), c.a);
}

/// …and scratch → frame. A plain copy: the quantize already happened, and doing
/// it on the way back would step an already-stepped value a second time.
///
/// The discard is the guard on the one way this pass could do damage. It covers
/// the whole attachment rather than a viewport, because a viewport is validated
/// against the attachment and a frame that reported itself *larger* than its own
/// texture would panic instead of merely looking wrong. That leaves the other
/// direction — a frame reporting itself *smaller* — where the fragments past the
/// quantized corner would read scratch that was never written this pass and paint
/// the rest of the picture with it. Discarding there keeps whatever the frame
/// already held, so a size the caller got wrong stays a cosmetic mismatch in one
/// corner instead of a wiped screen.
@fragment
fn fs_copy(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<i32>(pos.xy);
    if (pos.x >= p.frame.x || pos.y >= p.frame.y) {
        discard;
    }
    return textureLoad(src, px, 0);
}
