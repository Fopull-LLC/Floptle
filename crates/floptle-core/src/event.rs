//! A tiny double-buffered event channel — the seam for the engine/input/dialogue
//! event bus (`docs/subsystems/scene-and-nodes.md`).
//!
//! Double-buffering gives every reader exactly one frame to observe an event
//! regardless of ordering: `send` during frame N, everyone `drain`s at the top of
//! frame N+1, then the buffer swaps. The richer typed-bus + input/dialogue wiring
//! layers on this in later phases; the contract (write this frame, read next) is
//! the part worth pinning now.

/// A per-type event queue. `T` is the event payload (e.g. a `WindowResized`).
pub struct Events<T> {
    this_frame: Vec<T>,
    last_frame: Vec<T>,
}

impl<T> Default for Events<T> {
    fn default() -> Self {
        Self { this_frame: Vec::new(), last_frame: Vec::new() }
    }
}

impl<T> Events<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an event to be readable next frame.
    pub fn send(&mut self, event: T) {
        this_frame_push(self, event);
    }

    /// Read everything sent last frame.
    pub fn drain(&self) -> impl Iterator<Item = &T> {
        self.last_frame.iter()
    }

    /// Advance one frame: last frame's events expire, this frame's become readable.
    /// Call once per frame at the top of the loop.
    pub fn update(&mut self) {
        self.last_frame.clear();
        std::mem::swap(&mut self.this_frame, &mut self.last_frame);
    }
}

#[inline]
fn this_frame_push<T>(events: &mut Events<T>, event: T) {
    events.this_frame.push(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_exactly_next_frame() {
        let mut e: Events<i32> = Events::new();
        e.send(7);
        // not yet visible — it was sent "this frame"
        assert_eq!(e.drain().count(), 0);
        e.update();
        assert_eq!(e.drain().copied().collect::<Vec<_>>(), vec![7]);
        e.update();
        // expires after one frame
        assert_eq!(e.drain().count(), 0);
    }
}
