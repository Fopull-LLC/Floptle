//! The Lua `camera.*` API — project a world point to the active game camera's
//! screen pixels (and the inverse: a world ray from a screen pixel). The editor
//! feeds this the active game camera's **camera-relative** view-projection (no
//! translation, ADR-0015), its absolute world position, and the game viewport
//! rect in PHYSICAL pixels each frame — the same numbers the renderer and
//! `input.mouse()` use. All the picking LOGIC (nearest orbit point to the
//! cursor, hover, drag) stays game-side in Lua; the engine only exposes the
//! transform the renderer already knows.

use std::cell::RefCell;
use std::rc::Rc;

use glam::{DVec3, Mat4, Vec4};
use mlua::Lua;

/// The active game camera + viewport for this frame (fed by the editor).
#[derive(Clone, Debug)]
pub struct ViewInfo {
    /// Camera-relative view-projection (`cam.view_proj(aspect)`), column-major.
    pub view_proj: [f32; 16],
    /// The camera's absolute world position (points are made camera-relative first).
    pub cam_world: [f64; 3],
    /// Game viewport rect in physical pixels (origin + size; matches `input.mouse()`).
    pub vp_x: f32,
    pub vp_y: f32,
    pub vp_w: f32,
    pub vp_h: f32,
    /// The camera's vertical field of view, radians — what turns "how many
    /// pixels tall is the view" into "how many pixels is a metre"
    /// (`floptle/0058`).
    pub fov_y: f32,
    /// False until the editor has fed a real camera (screen queries no-op).
    pub valid: bool,
}

impl Default for ViewInfo {
    fn default() -> Self {
        Self {
            view_proj: [0.0; 16],
            cam_world: [0.0; 3],
            vp_x: 0.0,
            vp_y: 0.0,
            vp_w: 0.0,
            vp_h: 0.0,
            fov_y: 60f32.to_radians(),
            valid: false,
        }
    }
}

pub(crate) fn install_camera_api(lua: &Lua, view: Rc<RefCell<ViewInfo>>) {
    let Ok(t) = lua.create_table() else { return };

    // camera.exists() — true once the editor is feeding a live game camera.
    {
        let v = view.clone();
        if let Ok(f) = lua.create_function(move |_, ()| Ok(v.borrow().valid)) {
            let _ = t.set("exists", f);
        }
    }

    // camera.screenSize() -> w, h (the game viewport, physical pixels).
    {
        let v = view.clone();
        if let Ok(f) = lua.create_function(move |_, ()| {
            let v = v.borrow();
            Ok((v.vp_w, v.vp_h))
        }) {
            let _ = t.set("screenSize", f);
        }
    }

    // camera.pixelsPerUnit([distance]) -> px (`floptle/0058`).
    //
    // Every 2D project was deriving this itself out of the FOV and the camera's
    // Z — one project's copy reads `240 / (2 * 9.33 * tan 30°)` — and then
    // snapping the camera to a multiple of it. That constant is the engine's to
    // know: it is the projection, which the renderer already has.
    {
        let v = view.clone();
        if let Ok(f) = lua.create_function(move |_, distance: Option<f32>| {
            let v = v.borrow();
            if !v.valid {
                return Ok(0.0f32);
            }
            // Default to the camera's distance from the world origin — a flat
            // game is built around the origin plane, and asking for the number
            // at "where my game is" should not require restating where that is.
            let d = distance.unwrap_or_else(|| {
                let c = DVec3::from(v.cam_world);
                c.length() as f32
            });
            let half = (v.fov_y * 0.5).tan().max(1e-6);
            // The world height the view covers at `d`, against its pixel height.
            let world_h = 2.0 * d.abs().max(1e-6) * half;
            Ok(v.vp_h / world_h)
        }) {
            let _ = t.set("pixelsPerUnit", f);
        }
    }

    // camera.screenRect() -> x, y, w, h — the game viewport IN THE SAME SPACE as
    // input.mouse() and worldToScreen (which both carry the viewport's offset).
    // `screenSize` alone can't answer "is the cursor over the game view?": in the
    // editor the view is a dock panel, so the cursor's x is offset by whatever is
    // to its left, and comparing it against the WIDTH reads as "past the right
    // edge" from the moment you open the panel. That is an edge-pan camera that
    // slides away forever.
    {
        let v = view.clone();
        if let Ok(f) = lua.create_function(move |_, ()| {
            let v = v.borrow();
            Ok((v.vp_x, v.vp_y, v.vp_w, v.vp_h))
        }) {
            let _ = t.set("screenRect", f);
        }
    }

    // camera.worldToScreen(x,y,z) -> sx, sy, depth, onscreen.
    // Pixels are in the SAME space as input.mouse() (game-view physical px).
    // `onscreen` is false for points behind the camera or outside the frustum —
    // Lua should skip those when finding the nearest orbit point to the cursor.
    {
        let v = view.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z): (f64, f64, f64)| {
            let v = v.borrow();
            if !v.valid {
                return Ok((0.0f32, 0.0f32, 0.0f32, false));
            }
            let rel = (DVec3::new(x, y, z) - DVec3::from(v.cam_world)).as_vec3();
            let clip = Mat4::from_cols_array(&v.view_proj) * rel.extend(1.0);
            if clip.w <= 1e-4 {
                return Ok((0.0f32, 0.0f32, 0.0f32, false)); // behind the camera
            }
            let ndc = clip.truncate() / clip.w;
            let sx = v.vp_x + (ndc.x * 0.5 + 0.5) * v.vp_w;
            let sy = v.vp_y + (1.0 - (ndc.y * 0.5 + 0.5)) * v.vp_h;
            let onscreen = ndc.x >= -1.0 && ndc.x <= 1.0 && ndc.y >= -1.0 && ndc.y <= 1.0;
            Ok((sx, sy, ndc.z, onscreen))
        }) {
            let _ = t.set("worldToScreen", f);
        }
    }

    // camera.screenToRay(sx,sy) -> ox,oy,oz, dx,dy,dz — a world-space ray from
    // the cursor (origin on the near plane, unit direction into the scene).
    {
        let v = view.clone();
        if let Ok(f) = lua.create_function(move |_, (sx, sy): (f32, f32)| {
            let v = v.borrow();
            if !v.valid || v.vp_w <= 0.0 || v.vp_h <= 0.0 {
                return Ok((0.0f64, 0.0, 0.0, 0.0, 0.0, 1.0));
            }
            let nx = ((sx - v.vp_x) / v.vp_w) * 2.0 - 1.0;
            let ny = 1.0 - ((sy - v.vp_y) / v.vp_h) * 2.0;
            let inv = Mat4::from_cols_array(&v.view_proj).inverse();
            let near = inv * Vec4::new(nx, ny, 0.0, 1.0); // wgpu near plane z=0
            let far = inv * Vec4::new(nx, ny, 1.0, 1.0);
            let np = near.truncate() / near.w; // camera-relative
            let fp = far.truncate() / far.w;
            let dir = (fp - np).normalize_or_zero();
            let o = DVec3::from(v.cam_world) + np.as_dvec3();
            Ok((o.x, o.y, o.z, dir.x as f64, dir.y as f64, dir.z as f64))
        }) {
            let _ = t.set("screenToRay", f);
        }
    }

    let _ = lua.globals().set("camera", t);
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{Quat, Vec3};

    /// Build a camera-relative view-projection exactly like `RenderCamera::view_proj`:
    /// projection * (rotation-only view), i.e. no translation (ADR-0015).
    fn view_info(cam_world: [f64; 3], rot: Quat, w: f32, h: f32) -> ViewInfo {
        let proj = Mat4::perspective_rh(1.0, w / h, 0.1, 10_000.0);
        let view = Mat4::from_quat(rot.conjugate());
        ViewInfo {
            view_proj: (proj * view).to_cols_array(),
            cam_world,
            vp_x: 0.0,
            vp_y: 0.0,
            vp_w: w,
            vp_h: h,
            // The same 1.0 rad the projection above was built with, so
            // `pixelsPerUnit` and `worldToScreen` describe one camera.
            fov_y: 1.0,
            valid: true,
        }
    }

    /// `pixelsPerUnit` has to agree with the projection it came from: a metre
    /// at the camera's distance must measure the same as `worldToScreen` says
    /// it does. Otherwise a game snapping to it snaps to the wrong grid, which
    /// is exactly the hand-rolled constant this replaces.
    #[test]
    fn pixels_per_unit_matches_what_world_to_screen_measures() {
        // Camera 10 units back down +Z, looking at the origin.
        let lua = lua_with(view_info([0.0, 0.0, 10.0], Quat::IDENTITY, 800.0, 600.0));
        let px: f32 = lua.load("return camera.pixelsPerUnit()").eval().unwrap();

        // Measure the same metre by projecting two points a metre apart on the
        // plane the default distance refers to (the origin).
        let (ax, _, _, _): (f32, f32, f32, bool) =
            lua.load("return camera.worldToScreen(0, 0, 0)").eval().unwrap();
        let (bx, _, _, _): (f32, f32, f32, bool) =
            lua.load("return camera.worldToScreen(1, 0, 0)").eval().unwrap();
        let measured = (bx - ax).abs();
        assert!(
            (px - measured).abs() < 0.5,
            "pixelsPerUnit says {px} px/unit but projecting a unit measures {measured}"
        );

        // Twice as far away is half the size — the thing a flat game relies on.
        let far = lua_with(view_info([0.0, 0.0, 20.0], Quat::IDENTITY, 800.0, 600.0));
        let px_far: f32 = far.load("return camera.pixelsPerUnit()").eval().unwrap();
        assert!((px_far * 2.0 - px).abs() < 0.5, "{px_far} should be half of {px}");

        // An explicit distance overrides the default.
        let same: f32 = lua.load("return camera.pixelsPerUnit(20)").eval().unwrap();
        assert!((same - px_far).abs() < 0.5);
    }

    fn lua_with(view: ViewInfo) -> Lua {
        let lua = Lua::new();
        install_camera_api(&lua, Rc::new(RefCell::new(view)));
        lua
    }

    #[test]
    fn optical_axis_point_lands_at_viewport_center() {
        // Camera far from the origin, looking down -Z; a point straight ahead
        // must project to the center regardless of the camera's world offset.
        let cam = [1000.0, -500.0, 300.0];
        let lua = lua_with(view_info(cam, Quat::IDENTITY, 800.0, 600.0));
        let target = format!(
            "return camera.worldToScreen({}, {}, {})",
            cam[0],
            cam[1],
            cam[2] - 10.0
        );
        let (sx, sy, _d, on): (f32, f32, f32, bool) = lua.load(&target).eval().unwrap();
        assert!(on, "point ahead must be on-screen");
        assert!((sx - 400.0).abs() < 0.5, "sx={sx} (want 400)");
        assert!((sy - 300.0).abs() < 0.5, "sy={sy} (want 300)");
    }

    #[test]
    fn behind_camera_is_offscreen() {
        let lua = lua_with(view_info([0.0, 0.0, 0.0], Quat::IDENTITY, 800.0, 600.0));
        // +Z is BEHIND a -Z-looking camera.
        let (_sx, _sy, _d, on): (f32, f32, f32, bool) =
            lua.load("return camera.worldToScreen(0, 0, 10)").eval().unwrap();
        assert!(!on, "a point behind the camera must be off-screen");
    }

    #[test]
    fn screen_to_ray_round_trips_world_to_screen() {
        let cam = [10.0, 20.0, -30.0];
        let vi = view_info(cam, Quat::IDENTITY, 1280.0, 720.0);
        let lua = lua_with(vi);
        // A world point off-axis: project it, unproject the pixel, and the ray
        // must point from the camera straight at it.
        let p = [cam[0] + 4.0, cam[1] - 2.5, cam[2] - 40.0];
        let w2s = format!("return camera.worldToScreen({},{},{})", p[0], p[1], p[2]);
        let (sx, sy, _d, on): (f32, f32, f32, bool) = lua.load(&w2s).eval().unwrap();
        assert!(on);
        let s2r = format!("return camera.screenToRay({sx},{sy})");
        let (ox, oy, oz, dx, dy, dz): (f64, f64, f64, f64, f64, f64) =
            lua.load(&s2r).eval().unwrap();
        let origin = Vec3::new(ox as f32, oy as f32, oz as f32);
        let dir = Vec3::new(dx as f32, dy as f32, dz as f32).normalize();
        let to_p = (Vec3::new(p[0] as f32, p[1] as f32, p[2] as f32) - origin).normalize();
        assert!(dir.dot(to_p) > 0.999, "ray {dir:?} should aim at the point {to_p:?}");
    }
}
