//! # floptle-matter
//!
//! The central idea: **nothing is a static box.** Any object can be told how it
//! behaves *as physical matter* — it can morph, blend into other geometry like
//! soup, stick to surfaces and stretch when pulled, and (later) tear into
//! stringy strands and split apart. One `MatterModel` per object, multiple
//! optimized backends, opt-in complexity — you only pay for the behavior you
//! reach for. See `docs/subsystems/deformable-matter.md` + ADR-0013.
//!
//! It sits on the shared field layer (`floptle-field`) and the SDF physics
//! (`floptle-physics`), so a deformed object stays cleanly collidable for free.
//!
//! Planned modules (the deformation tiers, cheapest → heaviest):
//! - `model`    : the `MatterModel` component — declares behavior + budget.
//! - `morph`    : GPU vertex/field displacement (noise/curves/field) — ~free.
//! - `csg`      : field blend/mix/reject between objects (smin/smax + rules).
//! - `softbody` : XPBD constraint solver (distance/volume/shape-match).
//! - `stick`    : adhesion/cohesion contacts — stretch springs with a max force.
//! - `fracture` : elastoplastic strain → yield → tear → strands (future).

/// How a piece of matter behaves physically. Higher tiers cost more; an object
/// uses the cheapest tier that achieves the desired look (see ADR-0013).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatterTier {
    /// Static geometry. No deformation cost.
    Rigid,
    /// GPU vertex/field displacement only (ambient morphing, "breathing" meshes).
    Morph,
    /// Field CSG — blend/mix/reject with neighbouring matter (the "soup").
    FieldBlend,
    /// XPBD soft body driven by a particle/constraint cage.
    SoftBody,
    /// Soft body + adhesion (sticky) and stretch-to-fracture (strands). Future.
    Viscoelastic,
}

/// Per-object stickiness: how strongly surfaces bond on contact, and how far
/// they stretch before the bond fails.
#[derive(Debug, Clone, Copy)]
pub struct Adhesion {
    /// Max restoring force a bond can carry before it snaps (→ stringy strands).
    pub strength: f32,
    /// Strain (stretch ratio) at which a bond breaks.
    pub break_strain: f32,
}
