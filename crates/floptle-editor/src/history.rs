//! Undo/redo: scene snapshots (plus terrain byte swaps), the coalescing
//! rules for inspector edits, and restore.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::World;
use floptle_scene::SceneDoc;
use crate::{Editor, Snapshot};

impl Editor {
    // ---- undo / redo (whole-scene snapshots) --------------------------------
    pub(crate) fn snapshot(&self) -> SceneDoc {
        floptle_scene::to_doc(self.scene_name.clone(), &self.world)
    }

    pub(crate) fn push_history(&mut self, snap: Snapshot) {
        // Play-mode changes are a simulation, not edits: Stop reverts the whole
        // world, so they must never become undo points (undoing after Stop
        // would re-apply discarded play-state) or mark the scene "unsaved".
        if self.playing {
            return;
        }
        self.history.redo.clear();
        self.history.undo.push(snap);
        while self.history.undo.len() > self.history.max {
            self.history.undo.remove(0);
        }
        self.scene_dirty = true; // any undo-able edit (scene or terrain) is unsaved
    }

    /// Record the current scene as an undo point (call BEFORE a discrete edit).
    /// A no-op during Play — see [`Self::push_history`].
    pub(crate) fn record(&mut self) {
        if self.playing {
            return;
        }
        let s = self.snapshot();
        self.push_history(Snapshot::Scene(s));
    }

    /// Open an edit session for undo coalescing (gizmo/inspector drag = one step),
    /// using this frame's pre-edit snapshot.
    pub(crate) fn begin_edit(&mut self) {
        if !self.editing {
            if let Some(snap) = self.frame_snapshot.take() {
                self.push_history(Snapshot::Scene(snap));
            }
            self.editing = true;
        }
    }

    pub(crate) fn restore(&mut self, doc: SceneDoc) {
        // Entities are respawned below — drop animator runtimes keyed by the old ones.
        self.anim.clear_instances();
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
    }

    /// Swap the live terrain field for serialized `bytes`, queuing a GPU re-upload.
    /// The terrain node carrying `id` (if any), for keyed undo/save.
    pub(crate) fn terrain_entity_of_id(&self, id: u32) -> Option<Entity> {
        self.terrains.keys().copied().find(|&e| {
            matches!(self.world.get::<Matter>(e), Some(Matter::Terrain { id: i }) if *i == id)
        })
    }

    /// Restore a terrain field (by id) from serialized `bytes`. Returns the current
    /// bytes first (for the redo/undo counterpart), or `None` if the id is gone.
    pub(crate) fn swap_terrain_bytes(&mut self, id: u32, bytes: &[u8]) -> Option<Vec<u8>> {
        let e = self.terrain_entity_of_id(id)?;
        let cur = self.terrains.get(&e).map(|t| t.to_bytes());
        if let Some(t) = floptle_field::Terrain::from_bytes(bytes) {
            self.terrains.insert(e, t);
            self.terrain_gpu_dirty = true;
        }
        cur
    }

    pub(crate) fn undo(&mut self) {
        if self.playing {
            return; // stop play before editing history
        }
        // Recording keeps previewed clip values live in the world — end it (and
        // restore the true scene) before a history snapshot swaps entities out
        // from under it.
        self.stop_recording();
        match self.history.undo.pop() {
            Some(Snapshot::Scene(prev)) => {
                let cur = self.snapshot();
                self.history.redo.push(Snapshot::Scene(cur));
                self.restore(prev);
            }
            Some(Snapshot::Terrain(id, prev)) => {
                if let Some(cur) = self.swap_terrain_bytes(id, &prev) {
                    self.history.redo.push(Snapshot::Terrain(id, cur));
                }
            }
            None => {}
        }
    }

    pub(crate) fn redo(&mut self) {
        if self.playing {
            return;
        }
        self.stop_recording(); // same as undo — see above
        match self.history.redo.pop() {
            Some(Snapshot::Scene(next)) => {
                let cur = self.snapshot();
                self.history.undo.push(Snapshot::Scene(cur));
                self.restore(next);
            }
            Some(Snapshot::Terrain(id, next)) => {
                if let Some(cur) = self.swap_terrain_bytes(id, &next) {
                    self.history.undo.push(Snapshot::Terrain(id, cur));
                }
            }
            None => {}
        }
    }
}
