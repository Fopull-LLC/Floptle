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

    /// Restore a terrain stroke's chunks (by id), returning the inverse record (for
    /// the redo/undo counterpart), or `None` if the id is gone. Only the touched
    /// chunks swap; their meshes re-extract, and the shadow proxy re-derives (bounds
    /// may have shrunk/grown across the swap).
    pub(crate) fn swap_terrain_chunks(
        &mut self,
        id: u32,
        undo: &floptle_field::ChunkUndo,
    ) -> Option<floptle_field::ChunkUndo> {
        let e = self.terrain_entity_of_id(id)?;
        let t = self.terrains.get_mut(&e)?;
        let inverse = t.field.apply_undo(undo);
        t.rebuild_shadow();
        let coords = undo.coords();
        // Undo during Play restores geometry the sim never saw — mirror it.
        self.mirror_terrain_chunks_to_sim(e, &coords);
        self.terrain_chunks_dirty.entry(e).or_default().extend(coords);
        self.terrain_gpu_dirty = true; // full atlas re-upload (proxy box may have moved)
        Some(inverse)
    }

    /// Swap a paint id's colors for `colors`, returning what was there — the exact
    /// shape of `swap_terrain_bytes`, so undo/redo is a value swap and the ECS is
    /// never touched (entity ids don't survive a Scene restore).
    pub(crate) fn swap_paint_colors(
        &mut self,
        id: u32,
        colors: &[Vec<[u8; 4]>],
    ) -> Option<Vec<Vec<[u8; 4]>>> {
        let blocks = self.paint_data.get(&id)?.clone();
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return None;
        };
        let mut cur = Vec::with_capacity(blocks.parts.len());
        for (i, &(base, count)) in blocks.parts.iter().enumerate() {
            cur.push(raster.paint_block(base, count));
            if let Some(c) = colors.get(i) {
                raster.paint_restore(gpu, base, c);
            }
        }
        self.vpaint_epoch += 1; // texture-paint mirrors resync (paint_tex)
        Some(cur)
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
                if let Some(cur) = self.swap_terrain_chunks(id, &prev) {
                    self.history.redo.push(Snapshot::Terrain(id, cur));
                }
            }
            Some(Snapshot::VertexPaint(id, prev)) => {
                if let Some(cur) = self.swap_paint_colors(id, &prev) {
                    self.history.redo.push(Snapshot::VertexPaint(id, cur));
                }
            }
            Some(Snapshot::TexPaint(entries)) => {
                if let Some(redo) = self.swap_tex_paint(entries) {
                    self.history.redo.push(redo);
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
                if let Some(cur) = self.swap_terrain_chunks(id, &next) {
                    self.history.undo.push(Snapshot::Terrain(id, cur));
                }
            }
            Some(Snapshot::VertexPaint(id, next)) => {
                if let Some(cur) = self.swap_paint_colors(id, &next) {
                    self.history.undo.push(Snapshot::VertexPaint(id, cur));
                }
            }
            Some(Snapshot::TexPaint(entries)) => {
                if let Some(undo) = self.swap_tex_paint(entries) {
                    self.history.undo.push(undo);
                }
            }
            None => {}
        }
    }

    /// Swap a texture-paint stroke's nodes between their snapshot state and the current
    /// one, returning the inverse snapshot (for the opposite stack). A `None` target for a
    /// node = "no paint before this stroke", so it REMOVES that node's paint entirely —
    /// undoing a first-ever stroke reveals the untouched node, which is the point. Removed
    /// nodes have no inverse (redo can't recreate a dropped canvas); if the whole stroke
    /// was removals, there's nothing to redo at all.
    fn swap_tex_paint(&mut self, entries: Vec<(u32, Option<Vec<Vec<u8>>>)>) -> Option<Snapshot> {
        let mut inverse = Vec::new();
        for (id, target) in entries {
            match target {
                Some(images) => {
                    if let Some(cur) = self.tex_paint_snapshot(id) {
                        self.tex_paint_restore(id, &images);
                        inverse.push((id, Some(cur)));
                    }
                }
                None => {
                    // Bind first so the world query's borrow ends before the &mut call.
                    let ent = self
                        .world
                        .query::<floptle_core::TexturePaint>()
                        .find(|(_, tp)| tp.id == id)
                        .map(|(e, _)| e);
                    if let Some(ent) = ent {
                        self.clear_texture_paint(ent);
                    }
                }
            }
        }
        (!inverse.is_empty()).then_some(Snapshot::TexPaint(inverse))
    }
}
