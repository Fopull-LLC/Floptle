//! # floptle-rules
//!
//! The meta-spine. A world's **laws** as first-class, serializable, inheritable
//! data: a `Lawset` (the rules of a region) bound to an SDF volume (a `Realm`),
//! resolved at any point by the inside-test the engine already does. Light, time,
//! gravity, matter, and scale are *axes* of one object, not separate subsystems —
//! so a Floptle world is a `lawset.ron` you can diff, hot-reload, hand to an AI,
//! and gift as "here are the laws — bend them." See
//! `docs/subsystems/world-rules.md` + `docs/subsystems/field-interaction.md`
//! (ADR-0018, ADR-0019).
//!
//! Deliberately **thin and pure-data**: depends only on core + field, and is
//! read-only to render/physics/matter (no dependency cycles). Build the seam now;
//! wire only the axes you can prove first — do not big-bang the full system.
//!
//! Planned modules:
//! - `lawset`      : the Lawset struct + law axes (enum-of-models + `Inherit`).
//! - `realm`       : a Lawset bound to an SDF volume; the realm tree.
//! - `resolve`     : `effective_at(p)` — inside-test resolution + `smin` crossfade,
//!   cached once per body per step.
//! - `interaction` : the field-interaction graph (edges: field A modulates B) —
//!   the *data* half of ADR-0019 (the executor is a runtime system).

/// A law axis is inherited from the parent realm or set to a named model.
/// Each axis is a small enum (not free parameters) to avoid "property soup".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GravityLaw {
    Inherit,
    Global,
    Sources,
    SdfSurface,
    DensityField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightLaw {
    Inherit,
    Conventional,
    Radiance,
    BentTransport,
    Media,
}

/// The set of laws that hold inside a realm. `Inherit`/`None` axes cost nothing.
#[derive(Debug, Clone, Copy)]
pub struct Lawset {
    pub gravity: GravityLaw,
    pub light: LightLaw,
    /// Local time rate `r` (`None` = inherit); 1.0 normal, 0.0 frozen (ADR-0017).
    pub time_rate: Option<f32>,
}

impl Lawset {
    /// The root "ordinary universe" everything inherits from.
    pub const ROOT: Lawset = Lawset {
        gravity: GravityLaw::Global,
        light: LightLaw::Conventional,
        time_rate: Some(1.0),
    };
}

/// A field that can participate in a coupling edge (ADR-0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Sdf,
    Density,
    Gravity,
    Light,
    Time,
    Temperature,
}

/// "field `from` modulates field `to`" with a gain — one authored edge of the
/// field-interaction graph, iterated at low cadence with damping by the runtime.
#[derive(Debug, Clone, Copy)]
pub struct InteractionEdge {
    pub from: Field,
    pub to: Field,
    pub gain: f32,
}
