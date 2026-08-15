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
        // A selection step is history but not an EDIT: picking a node changes
        // nothing the scene file would save — so it neither dirties the scene
        // nor clears the redo stack. (Clearing redo on a pick would mean
        // "undo a move, click another node to check something, Ctrl+Y" loses
        // the move — a click must never make an edit unrecoverable.)
        let selection_only = matches!(snap, Snapshot::Selection(_));
        if !selection_only {
            self.history.redo.clear();
        }
        self.history.undo.push(snap);
        // The cap counts EDIT steps — selection steps are a few bytes and ride
        // along free (with their own generous total, so a long click spree
        // can't quietly evict real edits or grow without bound).
        let heavy =
            |s: &Snapshot| !matches!(s, Snapshot::Selection(_));
        while self.history.undo.iter().filter(|s| heavy(s)).count() > self.history.max
            || self.history.undo.len() > self.history.max * 8
        {
            self.history.undo.remove(0);
        }
        if !selection_only {
            self.scene_dirty = true; // any undo-able edit (scene or terrain) is unsaved
            // Whatever the selection did this frame belongs to this step's
            // snapshot — don't also mint a Selection step for it.
            self.suppress_sel_step = true;
        }
    }

    /// Record the current scene as an undo point (call BEFORE a discrete edit).
    /// A no-op during Play — see [`Self::push_history`].
    pub(crate) fn record(&mut self) {
        if self.playing {
            return;
        }
        let s = self.snapshot();
        let sel = self.selection_refs();
        self.push_history(Snapshot::Scene(s, sel));
    }

    /// Open an edit session for undo coalescing (gizmo/inspector drag = one step),
    /// using this frame's pre-edit snapshot.
    pub(crate) fn begin_edit(&mut self) {
        if !self.editing {
            if let Some(snap) = self.frame_snapshot.take() {
                // The baseline IS the frame-start selection — pair it with the
                // frame-start scene so undoing the edit restores both.
                let sel = self.refs_of(&self.sel_baseline);
                self.push_history(Snapshot::Scene(snap, sel));
            }
            self.editing = true;
        }
    }

    // ---- selection in the history -------------------------------------------
    /// `sel` as indices into `query::<Matter>()` order — the order `to_doc`
    /// writes nodes in, so a ref survives the world respawn a Scene undo
    /// performs. Non-node entries (a dead entity) are simply dropped.
    /// One scan, no map: a selection is one or two entities, so testing each
    /// node against it beats building a whole-scene index to look two things
    /// up. Order is kept (the last ref is the primary) and dead entities drop.
    fn refs_of(&self, sel: &[Entity]) -> Vec<usize> {
        if sel.is_empty() {
            return Vec::new();
        }
        let mut out = vec![usize::MAX; sel.len()];
        for (i, (e, _)) in self.world.query::<Matter>().enumerate() {
            if let Some(k) = sel.iter().position(|&s| s == e) {
                out[k] = i;
            }
        }
        out.retain(|&i| i != usize::MAX);
        out
    }

    pub(crate) fn selection_refs(&self) -> Vec<usize> {
        self.refs_of(&self.selection)
    }

    /// Re-point the live selection at `refs`, resolved against the CURRENT
    /// world (call after any restore). Order is kept — the last ref is the
    /// primary — and anything `restore` already re-selected isn't duplicated.
    ///
    /// `replace` clears first: a Selection step IS the whole selection, while a
    /// Scene step's refs are laid over what `restore` already re-selected (the
    /// map sub-object node it kept hold of).
    ///
    /// Clears the same derived state a click does ([`Editor::select_single`]) —
    /// a bone or particle-track editor left open for a node that is no longer
    /// selected is an Inspector showing something nobody picked.
    fn apply_selection_refs(&mut self, refs: &[usize], replace: bool) {
        if replace {
            self.selection.clear();
        }
        self.vfx_ui.sel_track = None;
        self.bone_selection = None;
        let order: Vec<Entity> = self.world.query::<Matter>().map(|(e, _)| e).collect();
        for e in refs.iter().filter_map(|&i| order.get(i).copied()) {
            if !self.selection.contains(&e) {
                self.selection.push(e);
            }
        }
        self.sel_baseline = self.selection.clone();
    }

    /// Per-frame history boundary (call once, at the top of a frame): captures
    /// the pre-edit scene + selection that `begin_edit` coalesces against, and
    /// turns a selection change since the LAST boundary into its own
    /// [`Snapshot::Selection`] step — unless something already on the history
    /// (or an undo/restore/load) explains it.
    pub(crate) fn begin_history_frame(&mut self) {
        if !self.playing && !self.anim_ui.record {
            self.frame_snapshot =
                Some(floptle_scene::to_doc(self.scene_name.clone(), &self.world));
            let explained = std::mem::take(&mut self.suppress_sel_step);
            if !explained && self.selection != self.sel_baseline {
                let baseline = std::mem::take(&mut self.sel_baseline);
                let prev = self.refs_of(&baseline);
                // Both as refs: a diff of dead entities (a scene load replaced
                // the world) or a same-set change is a no-op step — undoing it
                // would change nothing, so it must not cost a Ctrl+Z.
                if prev != self.selection_refs() {
                    self.push_history(Snapshot::Selection(prev));
                }
            }
        } else {
            // Play/record frames never mint selection steps — and must not
            // leave a stale diff behind for the first edit-mode frame after.
            self.suppress_sel_step = false;
        }
        self.sel_baseline = self.selection.clone();
    }

    pub(crate) fn restore(&mut self, doc: SceneDoc) {
        // The selection is about to move for a reason that is not a user pick —
        // the next history boundary must not turn it into a Selection step.
        self.suppress_sel_step = true;
        // Entities are respawned below — drop animator runtimes keyed by the old ones.
        // The map sub-object selection is keyed by Entity but ADDRESSES a stable
        // geometry id, so it can be carried across the respawn (undo mid-edit
        // used to dump you out of the shape you were working on).
        let keep_map = self.map_sel.take();
        self.anim.clear_instances();
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        self.selection.clear();
        // Entities are respawned, so a bone selection's (mesh entity, node idx) is stale
        // — drop it (and the pivot-edit mode) so nothing dereferences the old handle.
        self.bone_selection = None;
        self.pivot_edit = false;
        self.grabbed = None;
        self.drag = None;
        self.map_drag = None;
        self.map_stroke = None;
        self.map_box = None;
        self.map_draw = None;
        // Re-bind the sub-object selection to the respawned node carrying the
        // same map id, and re-select that node so the tool stays where it was.
        if let Some(mut sel) = keep_map
            && let Some(e) = self.world.query::<Matter>().find_map(|(e, m)| {
                matches!(m, Matter::MapMesh { id } if *id == sel.id).then_some(e)
            })
        {
            sel.entity = e;
            if let Some(mesh) = self.maps.meshes.get(&sel.id) {
                sel.prune(mesh);
            }
            self.selection.push(e);
            self.map_sel = Some(sel);
        }
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
        self.touch_terrain_edit(e); // an eviction must save this field first
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

    /// Bank a map-geometry edit: the pre-edit mesh AND the paint that was on it,
    /// as ONE undo step. Call it with the mesh as it was before the op (the
    /// paint is still pre-edit at this point — it only re-attaches on the next
    /// `sync_map_paint`, which is a frame away).
    pub(crate) fn push_map_history(&mut self, id: u32, pre: floptle_map::MapMesh) {
        let paint = self.map_paint_stash(id, &pre).map(Box::new);
        self.push_history(Snapshot::MapMesh(id, pre, paint));
    }

    /// Restore a map mesh and hand its stashed paint to the next rebuild,
    /// returning the inverse step (the mesh + paint that were live).
    fn swap_map_step(
        &mut self,
        id: u32,
        mesh: &floptle_map::MapMesh,
        paint: Option<Box<crate::map_paint::MapPaintStash>>,
    ) -> Snapshot {
        let inverse = self
            .maps
            .meshes
            .get(&id)
            .cloned()
            .and_then(|live| self.map_paint_stash(id, &live))
            .map(Box::new);
        let cur = self.swap_map_mesh(id, mesh);
        if let Some(p) = paint {
            self.maps.paint_restore.insert(id, *p);
        }
        Snapshot::MapMesh(id, cur, inverse)
    }

    pub(crate) fn undo(&mut self) {
        if self.playing {
            return; // stop play before editing history
        }
        // Recording keeps previewed clip values live in the world — end it (and
        // restore the true scene) before a history snapshot swaps entities out
        // from under it.
        self.stop_recording();
        self.suppress_sel_step = true; // history moves the selection; it isn't a pick
        match self.history.undo.pop() {
            Some(Snapshot::Scene(prev, sel)) => {
                let cur = self.snapshot();
                let cur_sel = self.selection_refs();
                self.history.redo.push(Snapshot::Scene(cur, cur_sel));
                self.restore(prev);
                self.apply_selection_refs(&sel, false);
            }
            Some(Snapshot::Selection(prev)) => {
                let cur = self.selection_refs();
                self.history.redo.push(Snapshot::Selection(cur));
                self.apply_selection_refs(&prev, true);
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
            Some(Snapshot::MapMesh(id, prev, paint)) => {
                let inverse = self.swap_map_step(id, &prev, paint);
                self.history.redo.push(inverse);
            }
            None => {}
        }
    }

    pub(crate) fn redo(&mut self) {
        if self.playing {
            return;
        }
        self.stop_recording(); // same as undo — see above
        self.suppress_sel_step = true;
        match self.history.redo.pop() {
            Some(Snapshot::Scene(next, sel)) => {
                let cur = self.snapshot();
                let cur_sel = self.selection_refs();
                self.history.undo.push(Snapshot::Scene(cur, cur_sel));
                self.restore(next);
                self.apply_selection_refs(&sel, false);
            }
            Some(Snapshot::Selection(next)) => {
                let cur = self.selection_refs();
                self.history.undo.push(Snapshot::Selection(cur));
                self.apply_selection_refs(&next, true);
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
            Some(Snapshot::MapMesh(id, next, paint)) => {
                let inverse = self.swap_map_step(id, &next, paint);
                self.history.undo.push(inverse);
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

#[cfg(test)]
mod tests {
    use floptle_core::{Entity, Matter, Name, Transform};
    use crate::Editor;

    fn node(ed: &mut Editor, name: &str) -> Entity {
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(e, Name(name.into()));
        ed.world.insert(e, Matter::Empty);
        e
    }

    fn selected_names(ed: &Editor) -> Vec<String> {
        ed.selection
            .iter()
            .filter_map(|&e| ed.world.get::<Name>(e).map(|n| n.0.clone()))
            .collect()
    }

    /// The complaint this whole feature answers: "when I undo and that undo
    /// involves a node, it always deselects the node". Undoing an edit must
    /// leave you holding the node you were editing.
    #[test]
    fn undoing_an_edit_keeps_the_node_selected() {
        let mut ed = Editor::default();
        let a = node(&mut ed, "A");
        ed.begin_history_frame();
        ed.select_single(a);
        ed.begin_history_frame(); // the pick becomes its own step
        ed.record(); // a discrete edit begins
        if let Some(t) = ed.world.get_mut::<Transform>(a) {
            t.translation.x = 5.0;
        }
        ed.begin_history_frame();

        ed.undo(); // un-move
        let a = ed
            .world
            .query::<Matter>()
            .map(|(e, _)| e)
            .find(|&e| ed.world.get::<Name>(e).is_some_and(|n| n.0 == "A"))
            .expect("A survives the undo");
        assert_eq!(ed.world.get::<Transform>(a).unwrap().translation.x, 0.0);
        assert_eq!(selected_names(&ed), vec!["A"], "the undone node stays selected");

        ed.undo(); // now the pick itself
        assert!(ed.selection.is_empty(), "the step before the pick had nothing selected");

        ed.redo();
        assert_eq!(selected_names(&ed), vec!["A"], "redo re-picks");
        ed.redo();
        assert_eq!(selected_names(&ed), vec!["A"], "redo re-applies the move to the selected node");
        let a = ed.selection[0];
        assert_eq!(ed.world.get::<Transform>(a).unwrap().translation.x, 5.0);
    }

    /// Picks are undo steps — so Ctrl+Z can walk back through what you selected
    /// — but they are not EDITS: the scene file didn't change.
    #[test]
    fn a_pick_is_an_undo_step_but_not_an_edit() {
        let mut ed = Editor::default();
        let a = node(&mut ed, "A");
        let b = node(&mut ed, "B");
        ed.begin_history_frame();
        ed.select_single(a);
        ed.begin_history_frame();
        ed.select_single(b);
        ed.begin_history_frame();

        assert!(!ed.scene_dirty, "picking must not mark the scene unsaved");
        assert_eq!(ed.history.undo.len(), 2, "one step per pick");

        ed.undo();
        assert_eq!(selected_names(&ed), vec!["A"]);
        ed.undo();
        assert!(ed.selection.is_empty());
        ed.redo();
        assert_eq!(selected_names(&ed), vec!["A"]);
        ed.redo();
        assert_eq!(selected_names(&ed), vec!["B"]);
        assert!(!ed.scene_dirty, "round-tripping picks still isn't an edit");
    }

    /// A selection change that happened as PART of an edit (delete clears the
    /// selection) belongs to that edit's step — undoing brings the node back
    /// selected, and no extra Ctrl+Z is charged for the deselect.
    #[test]
    fn undoing_a_delete_brings_the_node_back_selected() {
        let mut ed = Editor::default();
        node(&mut ed, "A");
        let b = node(&mut ed, "B");
        ed.begin_history_frame();
        ed.select_single(b);
        ed.begin_history_frame();
        let steps_after_pick = ed.history.undo.len();

        ed.delete_selected();
        ed.begin_history_frame();
        assert_eq!(
            ed.history.undo.len(),
            steps_after_pick + 1,
            "delete is ONE step — its deselect must not mint another"
        );

        ed.undo();
        assert_eq!(selected_names(&ed), vec!["B"], "the deleted node is back, and in hand");
    }

    /// A pick must never make an edit unrecoverable: undo a move, click
    /// something else to look at it, and Ctrl+Y still brings the move back.
    #[test]
    fn clicking_around_after_an_undo_does_not_lose_the_redo() {
        let mut ed = Editor::default();
        let a = node(&mut ed, "A");
        let b = node(&mut ed, "B");
        ed.begin_history_frame();
        ed.select_single(a);
        ed.begin_history_frame();
        ed.record();
        if let Some(t) = ed.world.get_mut::<Transform>(a) {
            t.translation.x = 4.0;
        }
        ed.begin_history_frame();

        ed.undo(); // the move is undone
        // …then a look around: two picks, neither of them an edit.
        ed.select_single(b);
        ed.begin_history_frame();
        ed.select_single(a);
        ed.begin_history_frame();

        ed.redo();
        let a = ed
            .world
            .query::<Matter>()
            .map(|(e, _)| e)
            .find(|&e| ed.world.get::<Name>(e).is_some_and(|n| n.0 == "A"))
            .expect("A is still here");
        assert_eq!(
            ed.world.get::<Transform>(a).unwrap().translation.x,
            4.0,
            "the move must still be redoable after looking at another node"
        );
    }

    /// Swapping in a new world (a scene load) leaves a stale selection baseline
    /// behind. That diff resolves to nothing — it must not mint a no-op step
    /// that eats a Ctrl+Z.
    #[test]
    fn a_scene_swap_does_not_mint_a_junk_selection_step() {
        let mut ed = Editor::default();
        let a = node(&mut ed, "A");
        ed.begin_history_frame();
        ed.select_single(a);
        ed.begin_history_frame();

        // The shape of every load path: fresh world, cleared selection + history.
        ed.world = floptle_core::World::new();
        ed.selection.clear();
        ed.history = crate::History::default();
        ed.begin_history_frame();
        assert!(ed.history.undo.is_empty(), "a scene swap must not create an undo step");
    }
}
