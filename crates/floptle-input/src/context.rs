//! Input **contexts** — a prioritised stack of layers that can swallow input
//! before lower layers see it.
//!
//! This is how a dialogue box eats movement without the player controller ever
//! knowing a dialogue exists. Push `"dialogue"` as a `Consume` layer enabling
//! only `Advance`/`Skip`; everything else resolves neutral until it's popped.
//!
//! The stack is plain data — no callbacks, nothing to "give input back". A
//! system that forgets to pop leaves an obvious, inspectable entry.

use serde::{Deserialize, Serialize};

use crate::map::InputMap;

/// Whether a layer lets unclaimed input fall through to lower layers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsumeMode {
    /// An overlay that listens for its own actions and lets the rest through
    /// (a HUD watching for `Pause`).
    #[default]
    Passthrough,
    /// A modal: actions this layer doesn't enable resolve neutral.
    Consume,
}

/// One layer of the stack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub name: String,
    /// Higher wins. Ties resolve by push order (later wins).
    pub priority: i32,
    /// Action and axis names this layer cares about.
    pub enabled: Vec<String>,
    pub mode: ConsumeMode,
}

impl Context {
    pub fn consuming(name: impl Into<String>, priority: i32, enabled: &[&str]) -> Self {
        Self {
            name: name.into(),
            priority,
            enabled: enabled.iter().map(|s| s.to_string()).collect(),
            mode: ConsumeMode::Consume,
        }
    }
}

/// Which actions and axes are currently allowed to resolve. Bit `i` set means
/// action/axis `i` reads normally; clear means it reads neutral.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllowMask {
    pub actions: u64,
    pub axes1: u64,
    pub axes2: u64,
}

impl AllowMask {
    /// Nothing blocked — what an empty stack produces.
    pub const ALL: AllowMask =
        AllowMask { actions: u64::MAX, axes1: u64::MAX, axes2: u64::MAX };
}

impl Default for AllowMask {
    fn default() -> Self {
        AllowMask::ALL
    }
}

/// The live stack.
#[derive(Clone, Debug, Default)]
pub struct ContextStack {
    layers: Vec<Context>,
}

impl ContextStack {
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.layers.iter().map(|c| c.name.as_str())
    }

    /// Push a layer. Pushing a name that's already on the stack replaces it, so
    /// a script that opens the same menu twice can't leak a duplicate.
    pub fn push(&mut self, ctx: Context) {
        self.layers.retain(|c| c.name != ctx.name);
        self.layers.push(ctx);
    }

    /// Pop by name. Returns whether anything was removed.
    pub fn pop(&mut self, name: &str) -> bool {
        let before = self.layers.len();
        self.layers.retain(|c| c.name != name);
        self.layers.len() != before
    }

    pub fn clear(&mut self) {
        self.layers.clear();
    }

    /// Resolve the stack into a mask for `map`.
    ///
    /// Only the **highest-priority `Consume`** layer can block: everything it
    /// doesn't enable goes neutral, except what a still-higher layer enables.
    /// `Passthrough` layers never block — they exist to declare intent and to
    /// sit above a consumer.
    pub fn allow_mask(&self, map: &InputMap) -> AllowMask {
        // Sort indices by priority (stable, so later pushes win ties).
        let mut order: Vec<&Context> = self.layers.iter().collect();
        order.sort_by_key(|c| c.priority);
        let Some(blocker_pos) = order.iter().rposition(|c| c.mode == ConsumeMode::Consume) else {
            return AllowMask::ALL;
        };

        // The blocker and everything above it contribute their enabled names.
        let mut allow = AllowMask { actions: 0, axes1: 0, axes2: 0 };
        for ctx in &order[blocker_pos..] {
            for name in &ctx.enabled {
                if let Some(i) = map.action_index(name) {
                    allow.actions |= 1u64 << i;
                }
                if let Some(i) = map.axis1_index(name) {
                    allow.axes1 |= 1u64 << i;
                }
                if let Some(i) = map.axis2_index(name) {
                    allow.axes2 |= 1u64 << i;
                }
            }
        }
        allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Action, Axis2, Socd};

    fn map() -> InputMap {
        InputMap {
            actions: vec![Action::new("Jump"), Action::new("Advance"), Action::new("Pause")],
            axes2: vec![Axis2 { name: "Move".into(), socd: Socd::Neutral, bindings: vec![] }],
            ..Default::default()
        }
    }

    #[test]
    fn empty_stack_allows_everything() {
        assert_eq!(ContextStack::default().allow_mask(&map()), AllowMask::ALL);
    }

    #[test]
    fn passthrough_alone_blocks_nothing() {
        let m = map();
        let mut s = ContextStack::default();
        s.push(Context {
            name: "hud".into(),
            priority: 10,
            enabled: vec!["Pause".into()],
            mode: ConsumeMode::Passthrough,
        });
        assert_eq!(s.allow_mask(&m), AllowMask::ALL);
    }

    #[test]
    fn a_consuming_layer_blocks_everything_it_does_not_enable() {
        let m = map();
        let mut s = ContextStack::default();
        s.push(Context::consuming("dialogue", 100, &["Advance"]));
        let mask = s.allow_mask(&m);
        assert_eq!(mask.actions, 1 << m.action_index("Advance").unwrap());
        assert_eq!(mask.axes2, 0, "movement is swallowed while dialogue is up");
    }

    #[test]
    fn a_higher_passthrough_still_gets_its_action_through_a_consumer() {
        // A pause overlay above a modal dialogue: Pause must keep working.
        let m = map();
        let mut s = ContextStack::default();
        s.push(Context::consuming("dialogue", 100, &["Advance"]));
        s.push(Context {
            name: "hud".into(),
            priority: 200,
            enabled: vec!["Pause".into()],
            mode: ConsumeMode::Passthrough,
        });
        let mask = s.allow_mask(&m);
        assert_ne!(mask.actions & (1 << m.action_index("Pause").unwrap()), 0);
        assert_ne!(mask.actions & (1 << m.action_index("Advance").unwrap()), 0);
        assert_eq!(mask.actions & (1 << m.action_index("Jump").unwrap()), 0);
    }

    #[test]
    fn the_highest_priority_consumer_wins() {
        // Menu over dialogue: the menu's set applies, dialogue's does not.
        let m = map();
        let mut s = ContextStack::default();
        s.push(Context::consuming("dialogue", 50, &["Advance"]));
        s.push(Context::consuming("menu", 150, &["Pause"]));
        let mask = s.allow_mask(&m);
        assert_ne!(mask.actions & (1 << m.action_index("Pause").unwrap()), 0);
        assert_eq!(mask.actions & (1 << m.action_index("Advance").unwrap()), 0);
    }

    #[test]
    fn pushing_the_same_name_twice_does_not_leak() {
        let mut s = ContextStack::default();
        s.push(Context::consuming("menu", 10, &["Pause"]));
        s.push(Context::consuming("menu", 10, &["Pause"]));
        assert_eq!(s.names().count(), 1);
        assert!(s.pop("menu"));
        assert!(s.is_empty());
        assert!(!s.pop("menu"), "popping a missing layer reports false");
    }

    #[test]
    fn unknown_names_in_a_context_are_ignored() {
        let m = map();
        let mut s = ContextStack::default();
        s.push(Context::consuming("typo", 10, &["Jmup"]));
        assert_eq!(s.allow_mask(&m).actions, 0, "a typo blocks rather than opens");
    }
}
