//! Joints: a node's body held to another node, or to the world, by a hinge,
//! a weld, a rope, a spring, a slider or a wheel.
//!
//! A joint goes on the part that moves and names what it hangs from by its
//! node name, the way a 2D camera names what it follows: a door names its
//! frame, each wheel names the car, a link names the link above. So a car
//! has as many wheels as it has wheel nodes. Leave the name empty and the
//! joint holds the node to the place it started: a hanging sign's hook, a
//! pendulum's pivot.

/// What a joint holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JointKind {
    /// A pin: the two anchors stay together and the bodies turn about it
    /// freely, or between `min` and `max` degrees. A door, a flipper, a
    /// ragdoll's elbow. `motor` turns this node against the other.
    #[default]
    Hinge,
    /// The two bodies move as one: a weld.
    Fixed,
    /// The anchors stay exactly `length` apart: a rod.
    Distance,
    /// The anchors stay at most `length` apart, and closer is slack: a rope,
    /// a chain, a leash.
    Rope,
    /// The anchors are pulled toward `length` apart by `stiffness`, losing
    /// speed to `damping`: a spring, a bungee, a soft link.
    Spring,
    /// This node slides along the other's `axis` without turning, between
    /// `min` and `max` units of its starting place: a piston, a lift, a
    /// sliding door. `motor` drives it along.
    Slider,
    /// This node is a wheel on the other: free to turn, held at its mount
    /// along the other's `axis` by a suspension of `stiffness` and
    /// `damping`. `motor` drives it round.
    Wheel,
    /// The anchor is pulled toward a point a script sets, with a pull of
    /// `stiffness` and no more force than `max_force`: picking things up and
    /// dragging them about.
    Target,
}

impl JointKind {
    pub const ALL: [JointKind; 8] = [
        JointKind::Hinge,
        JointKind::Fixed,
        JointKind::Distance,
        JointKind::Rope,
        JointKind::Spring,
        JointKind::Slider,
        JointKind::Wheel,
        JointKind::Target,
    ];

    pub fn name(self) -> &'static str {
        match self {
            JointKind::Hinge => "hinge",
            JointKind::Fixed => "fixed",
            JointKind::Distance => "distance",
            JointKind::Rope => "rope",
            JointKind::Spring => "spring",
            JointKind::Slider => "slider",
            JointKind::Wheel => "wheel",
            JointKind::Target => "target",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(s))
    }
}

/// A joint on this node's body. See [`JointKind`] for what each kind uses.
#[derive(Clone, Debug, PartialEq)]
pub struct Joint {
    pub kind: JointKind,
    /// The node held to, by name. Empty: the world, at the anchor's place
    /// when play starts.
    pub other: String,
    /// Where on this node, in its own space.
    pub anchor: [f32; 3],
    /// Where on the other node, in its own space. Ignored for the world.
    /// Left at zero, a hinge, weld, slider or wheel uses the point of the
    /// other node under `anchor` when play starts, so it joins the two where
    /// they were placed.
    pub other_anchor: [f32; 3],
    /// The direction a slider moves along and a wheel's suspension works
    /// along, and a 3D hinge's axle, in the other node's space (the world's,
    /// with none). Z, out of a 2D scene, by default: the hinge of every 2D
    /// game.
    pub axis: [f32; 3],
    /// Rest length for a distance, rope or spring joint. Below zero: the
    /// distance between the anchors when play starts.
    pub length: f32,
    /// Hold the turn (hinge, degrees) or travel (slider, units) between
    /// `min` and `max`.
    pub limit: bool,
    pub min: f32,
    pub max: f32,
    /// Drive this node round (hinge or wheel, degrees a second, anticlockwise
    /// positive) or along (slider, units a second) against the other, with
    /// no more than `max_force` behind it.
    pub motor: bool,
    pub motor_speed: f32,
    /// The most force (or torque) the motor or a target pull may use. Zero:
    /// as much as it takes.
    pub max_force: f32,
    /// How hard a spring, a wheel's suspension or a target pull, in cycles a
    /// second: 1 is soft, 5 firm, 20 nearly rigid. Mass does not change how
    /// a spring of a given number feels.
    pub stiffness: f32,
    /// How quickly a spring's bounce dies: 0 bounces for ever, 1 settles
    /// without overshooting.
    pub damping: f32,
    /// The joint breaks, and lets go for good, past this force. Zero: never.
    pub break_force: f32,
    /// The two bodies still collide with each other. Off by default: a
    /// hinged arm sits inside the shoulder it hangs from.
    pub collide_connected: bool,
    /// Set by the physics when the joint broke; clear it to mend the joint.
    /// Not saved.
    pub broken: bool,
    /// The largest force the joint carried in the last step, set by the
    /// physics, for a script deciding how close it is to breaking. Not saved.
    pub force: f32,
    /// Where a target joint pulls toward, in world space. Set by a script.
    /// Not saved.
    pub target: Option<[f64; 3]>,
    /// The other node by entity index, when a script joined two nodes by
    /// handing over the node itself. Used ahead of `other`, so it works for a
    /// node with no name or one of several with the same name. Not saved.
    pub other_id: Option<u32>,
}

impl Default for Joint {
    fn default() -> Self {
        Self {
            kind: JointKind::Hinge,
            other: String::new(),
            anchor: [0.0; 3],
            other_anchor: [0.0; 3],
            axis: [0.0, 0.0, 1.0],
            length: -1.0,
            limit: false,
            min: -45.0,
            max: 45.0,
            motor: false,
            motor_speed: 90.0,
            max_force: 0.0,
            stiffness: 4.0,
            damping: 0.5,
            break_force: 0.0,
            collide_connected: false,
            broken: false,
            force: 0.0,
            target: None,
            other_id: None,
        }
    }
}
