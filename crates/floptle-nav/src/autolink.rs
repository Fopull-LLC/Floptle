//! The links the bake finds for itself: dropping off a ledge, and jumping a gap.
//!
//! A navmesh is a surface, and its edges are where the surface stops. Most of
//! those edges are walls — the route goes round. But some of them are a
//! knee-high ledge with the same floor a step below, or a half-metre gap with
//! the corridor continuing on the other side, and a character that treats those
//! as walls is a character that gets lost in a level a person walks through
//! without noticing.
//!
//! Placing an [`OffLink`] by hand at each one is not an answer. A room has
//! dozens; a level has hundreds; and every one of them moves the moment somebody
//! moves the geometry. So the bake works them out, from the only two numbers
//! that decide it: [`NavSettings::max_drop`] and [`NavSettings::max_jump`].
//!
//! # What it looks for
//!
//! Every cell the flood fill stopped at — a cell with nothing to step to in some
//! direction — is a ledge in that direction. From each one the search walks
//! **outward, column by column**, and takes the first ground it can land on:
//!
//! | it finds | it makes |
//! |---|---|
//! | ground more than `step_height` below, no further down than `max_drop` | a one-way **drop** |
//! | ground within `step_height` either way, no further across than `max_jump` | a two-way **jump** |
//! | ground more than `step_height` **above** | nothing — this cannot invent a character that climbs |
//!
//! and it stops looking the moment a column has something standing up in it,
//! because that is a wall and you cannot walk through a wall by falling.
//!
//! # A carve is not a gap
//!
//! Ground a designer carved out with a blocking [`AreaVolume`] leaves a hole in
//! the walkable surface that looks, from here, exactly like a chasm. It is not:
//! a carve is the designer saying *nothing may walk here*, and a bake that
//! answered it by inventing a hop over the top would be a tool overruling the
//! person using it. Any candidate whose line passes through a blocking volume is
//! thrown away.
//!
//! # Only where walking cannot already get there
//!
//! A candidate whose two ends are in the same region is thrown away. Both ends
//! being in one region *means* there is already a way to walk between them, so
//! the link would be a shortcut rather than a connection — and a level's ledges
//! offer tens of thousands of shortcuts, every one of which costs the search
//! time on every query for the rest of the game. Connections are the ones worth
//! having and they are the ones that were missing.
//!
//! # Spacing
//!
//! A twenty-metre balcony would otherwise produce a drop link per cell — five
//! hundred of them, all saying the same thing. Once a link is taken, nothing
//! within [`spacing`] of its mouth (in the same direction) is taken again, so a
//! long ledge comes out as a handful of evenly spread ways down.

use std::collections::HashSet;

use crate::heightfield::Heightfield;
use crate::link::{LinkKind, OffLink};
use crate::walkable::WalkableGrid;
use crate::{AreaVolume, NavSettings};

/// The four ways off a ledge. The same four the grid steps in, so "there is
/// nothing to step to that way" and "look that way" are the same question.
const OUT: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

/// The most links one bake will invent.
///
/// Not a guess at what is reasonable — a guard against the pathological level
/// that would otherwise hand the router a hundred thousand edges and make every
/// query slow for ever. A bake that hits it says so.
pub const MAX_GENERATED: usize = 4096;

/// How far apart two generated links have to be, in metres.
///
/// Wide enough that a balcony gets a few ways down rather than one per cell,
/// narrow enough that a character never has to walk far along a ledge to reach
/// one. Scaled off the body, because "far" means something different to a rat
/// and to a mech, and floored so a tiny agent on a fine grid does not carpet the
/// level.
pub fn spacing(settings: &NavSettings) -> f32 {
    (settings.agent_radius * 4.0).clamp(1.0, 4.0)
}

/// What one bake's link generation did — the counts the editor reports, so
/// "nothing happened" and "it is switched off" are different sentences.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub drops: usize,
    pub jumps: usize,
    /// The cap was reached and candidates were left on the table. A level this
    /// says yes to is a level whose numbers want looking at.
    pub capped: bool,
}

impl Found {
    pub fn total(&self) -> usize {
        self.drops + self.jumps
    }
}

/// Work out the drops and jumps in a baked grid.
///
/// `first_id` is where generated ids start: hand-placed links own their ids and
/// two links a script cannot tell apart is the bug this avoids. The caller
/// passes one past the highest it placed.
///
/// The heightfield comes in as well as the grid because the grid only knows
/// where you can *stand*, and the question "is there a wall in the way" is about
/// everything else in the column.
pub fn generate(
    grid: &WalkableGrid,
    field: &Heightfield,
    settings: &NavSettings,
    volumes: &[AreaVolume],
    first_id: u32,
) -> (Vec<OffLink>, Found) {
    let mut found = Found::default();
    let cell = grid.cell_size;
    let drop = settings.max_drop.max(0.0);
    let jump = settings.max_jump.max(0.0);
    let step = settings.step_height.max(0.0);
    if cell <= 0.0 || (drop <= 0.0 && jump <= 0.0) {
        return (Vec::new(), found);
    }

    // How far across a drop is allowed to be. Erosion has already pulled BOTH
    // mouths back by the agent's radius, so even a sheer drop off a wall has the
    // agent's own width twice over between its ends — measuring a drop as if it
    // were vertical would find none of them.
    let drop_span = jump.max(settings.agent_radius * 2.0 + cell * 2.0);
    let reach = ((drop_span.max(jump) / cell).ceil() as i32 + 1).clamp(1, 96);

    let gap_step = spacing(settings);
    // Heights are bucketed too: a stairwell has one plan position and six
    // landings, and they are not the same ledge.
    let rise_step = step.max(0.5);
    let mut taken: HashSet<(i32, i32, i32, i8, i8)> = HashSet::new();
    let mut out: Vec<OffLink> = Vec::new();
    // Where a link's mouth falls in the suppression grid, facing that way.
    let key = |at: [f32; 3], dx: i32, dz: i32| {
        (
            (at[0] / gap_step).floor() as i32,
            (at[1] / rise_step).floor() as i32,
            (at[2] / gap_step).floor() as i32,
            dx as i8,
            dz as i8,
        )
    };

    for i in 0..grid.cells.len() {
        if out.len() >= MAX_GENERATED {
            found.capped = true;
            break;
        }
        let here = grid.world_of(i);
        let region = grid.region[i];
        for (dx, dz) in OUT {
            // Somewhere to walk that way is not a ledge.
            if grid.steps_to(i, dx, dz) {
                continue;
            }
            let mine = key(here, dx, dz);
            if taken.contains(&mine) {
                continue;
            }
            let Some((j, kind)) = probe(grid, field, settings, i, dx, dz, reach, drop, jump, step, drop_span)
            else {
                continue;
            };
            // Already walkable between the two: this would be a shortcut, and a
            // level's ledges offer more shortcuts than the router can afford.
            if grid.region[j] == region {
                continue;
            }
            let there = grid.world_of(j);
            // A hole somebody carved is not a gap to be hopped over.
            if crosses_a_carve(volumes, here, there) {
                continue;
            }
            taken.insert(mine);
            if kind == LinkKind::Jump {
                // A jump is two-way, so the far side looking back at us is THIS
                // crossing and not another one. Without this a chasm gets every
                // hop twice — once from each bank — and the router pays for
                // both on every query for the rest of the game.
                taken.insert(key(there, -dx, -dz));
            }
            let id = first_id.saturating_add(out.len() as u32);
            let n = out.len() + 1;
            let mut link = match kind {
                LinkKind::Drop => {
                    found.drops += 1;
                    OffLink::new(id, format!("drop {n}"), here, there)
                }
                _ => {
                    found.jumps += 1;
                    let mut l = OffLink::new(id, format!("jump {n}"), here, there);
                    l.bidirectional = true;
                    l
                }
            };
            link.kind = kind;
            out.push(link);
            if out.len() >= MAX_GENERATED {
                found.capped = true;
                break;
            }
        }
    }
    (out, found)
}

/// Look outward from one ledge cell and answer with the first place worth
/// landing, or nothing.
#[allow(clippy::too_many_arguments)]
fn probe(
    grid: &WalkableGrid,
    field: &Heightfield,
    settings: &NavSettings,
    i: usize,
    dx: i32,
    dz: i32,
    reach: i32,
    drop: f32,
    jump: f32,
    step: f32,
    drop_span: f32,
) -> Option<(usize, LinkKind)> {
    let cell = grid.cell_size;
    let from = grid.cells[i];
    for k in 1..=reach {
        // The gap a person would see: the far column's near face against this
        // column's far face, which is one column less than the centres are
        // apart. `k == 1` is two touching columns and no gap at all.
        let gap = (k - 1) as f32 * cell;
        let mut best: Option<(usize, LinkKind, f32)> = None;
        for j in grid.column_offset(i, dx, dz, k) {
            let dy = grid.cells[j].y - from.y;
            let kind = if dy > step {
                // Climbing. Nothing here knows whether this character can, and
                // inventing a character that scales walls is worse than a route
                // that goes round.
                continue;
            } else if dy < -step {
                if drop <= 0.0 || -dy > drop || gap > drop_span {
                    continue;
                }
                LinkKind::Drop
            } else {
                if jump <= 0.0 || gap > jump || gap <= 0.0 {
                    continue;
                }
                LinkKind::Jump
            };
            // The gentlest landing in the column: the shortest fall, the
            // flattest hop.
            if best.is_none_or(|(_, _, d)| dy.abs() < d) {
                best = Some((j, kind, dy.abs()));
            }
        }
        if let Some((j, kind, _)) = best {
            return Some((j, kind));
        }
        // Nothing to land on here — but if this column has something standing
        // up in it, there is a wall between us and anything further out, and a
        // wall is not a gap.
        if walled(field, grid, i, dx, dz, k, settings) {
            return None;
        }
    }
    None
}

/// Whether the straight line between two ends passes through ground a designer
/// carved out.
///
/// Sampled rather than solved: a blocking volume is an arbitrary rotated,
/// scaled box, and "does this segment intersect it" is a page of arithmetic
/// where "is any of these nine points inside it" is a line — and a carve narrow
/// enough to slip between nine samples of a link that is at most a few metres
/// long is narrower than the cells that made the hole in the first place.
fn crosses_a_carve(volumes: &[AreaVolume], from: [f32; 3], to: [f32; 3]) -> bool {
    if volumes.iter().all(|v| !v.blocks) {
        return false;
    }
    const SAMPLES: usize = 8;
    (0..=SAMPLES).any(|k| {
        let t = k as f32 / SAMPLES as f32;
        let at = [
            from[0] + (to[0] - from[0]) * t,
            from[1] + (to[1] - from[1]) * t,
            from[2] + (to[2] - from[2]) * t,
        ];
        volumes.iter().any(|v| v.blocks && v.contains(at))
    })
}

/// Whether the column `k` out from `i` has something in it tall enough to be in
/// the way.
///
/// "In the way" is deliberately narrow: taller than the ledge by more than a
/// step, and with its foot low enough to be at the character rather than a
/// ceiling well overhead. A parapet blocks; the roof three storeys up does not.
fn walled(
    field: &Heightfield,
    grid: &WalkableGrid,
    i: usize,
    dx: i32,
    dz: i32,
    k: i32,
    settings: &NavSettings,
) -> bool {
    let c = grid.cells[i];
    let (nx, nz) = (c.x as i32 + dx * k, c.z as i32 + dz * k);
    if nx < 0 || nz < 0 {
        return true;
    }
    let Some(col) = field.column(nx as usize, nz as usize) else {
        // Off the edge of the field. There is nothing out there to reach.
        return true;
    };
    let over = c.y + settings.step_height;
    let head = c.y + settings.agent_height;
    col.surfaces.iter().any(|s| s.y > over && s.base < head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bake, Tri};

    /// A `w`×`d` floor at height `y`, as two triangles.
    fn slab(x0: f32, z0: f32, w: f32, d: f32, y: f32) -> Vec<Tri> {
        vec![
            Tri::new([x0, y, z0], [x0 + w, y, z0], [x0, y, z0 + d]),
            Tri::new([x0 + w, y, z0], [x0 + w, y, z0 + d], [x0, y, z0 + d]),
        ]
    }

    /// Settings for a small, precise test character: it fits the slabs below,
    /// and the cell is fine enough that `cell_size_advice` is quiet.
    fn tester() -> NavSettings {
        NavSettings {
            agent_radius: 0.2,
            agent_height: 1.0,
            step_height: 0.3,
            cell_size: 0.1,
            ..Default::default()
        }
    }

    /// **The report this exists for.** A knee-high ledge is not an insurmountable
    /// obstacle, and a character standing on top of one has to be able to get
    /// down.
    #[test]
    fn a_ledge_within_the_drop_becomes_a_one_way_link() {
        let s = NavSettings { max_drop: 1.5, max_jump: 0.0, ..tester() };
        // A high floor and a low one, touching in plan — a step 1 m tall.
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 1.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.0));
        let mesh = bake(&tris, &s).expect("both floors are walkable");

        let drops: Vec<_> =
            mesh.off_links.iter().filter(|l| l.kind == LinkKind::Drop).collect();
        assert!(!drops.is_empty(), "a 1 m ledge with 1.5 m of drop must be linked");
        assert!(drops.iter().all(|l| !l.bidirectional), "falling is one way");
        assert!(drops.iter().all(|l| l.resolved()), "and both ends must be on the mesh");
        assert!(
            drops.iter().all(|l| l.from[1] > l.to[1]),
            "a drop goes down: {:?}",
            drops.iter().map(|l| (l.from[1], l.to[1])).collect::<Vec<_>>()
        );

        // The route now exists, and it knows it is dropping.
        let path = mesh.path([1.0, 1.0, 2.0], [7.0, 0.0, 2.0]).expect("both ends are on the mesh");
        assert!(path.complete, "the drop has to join the two floors");
        assert_eq!(path.crossings.len(), 1, "and the walk reports the crossing");
    }

    /// The same geometry with the drop turned down is two islands again — which
    /// is the pair that makes the number mean something.
    #[test]
    fn a_ledge_taller_than_the_drop_is_still_a_wall() {
        let s = NavSettings { max_drop: 0.5, max_jump: 0.0, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 1.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.0));
        let mesh = bake(&tris, &s).unwrap();
        assert!(mesh.off_links.is_empty(), "a 1 m ledge is beyond a 0.5 m drop");
        let path = mesh.path([1.0, 1.0, 2.0], [7.0, 0.0, 2.0]).unwrap();
        assert!(!path.complete, "and so the far floor is unreachable");
    }

    /// A gap you could step across is a gap, not a chasm — and unlike a drop it
    /// works both ways.
    #[test]
    fn a_gap_within_the_jump_becomes_a_two_way_link() {
        let s = NavSettings { max_drop: 0.0, max_jump: 1.2, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(4.6, 0.0, 4.0, 4.0, 0.0)); // 0.6 m of nothing between
        let mesh = bake(&tris, &s).unwrap();

        let jumps: Vec<_> =
            mesh.off_links.iter().filter(|l| l.kind == LinkKind::Jump).collect();
        assert!(!jumps.is_empty(), "a 0.6 m gap is inside a 1.2 m jump");
        assert!(jumps.iter().all(|l| l.bidirectional), "a gap you can clear you can clear back");

        let there = mesh.path([1.0, 0.0, 2.0], [7.0, 0.0, 2.0]).unwrap();
        let back = mesh.path([7.0, 0.0, 2.0], [1.0, 0.0, 2.0]).unwrap();
        assert!(there.complete && back.complete, "both ways");
    }

    /// A wall is not a gap. The same two floors with something solid between
    /// them must not be jumped, however narrow the wall is.
    #[test]
    fn a_wall_between_two_floors_is_never_jumped() {
        let s = NavSettings { max_drop: 1.5, max_jump: 1.5, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(4.6, 0.0, 4.0, 4.0, 0.0));
        // A 2 m wall standing in the 0.6 m gap, as a box.
        let (x0, x1) = (4.1, 4.5);
        for (a, b) in [([x0, 0.0], [x0, 4.0]), ([x1, 0.0], [x1, 4.0])] {
            tris.push(Tri::new([a[0], 0.0, a[1]], [b[0], 0.0, b[1]], [a[0], 2.0, a[1]]));
            tris.push(Tri::new([b[0], 0.0, b[1]], [b[0], 2.0, b[1]], [a[0], 2.0, a[1]]));
        }
        tris.extend(slab(x0, 0.0, x1 - x0, 4.0, 2.0)); // its top
        let mesh = bake(&tris, &s).unwrap();
        let across: Vec<_> = mesh
            .off_links
            .iter()
            .filter(|l| (l.from[0] - 4.3).signum() != (l.to[0] - 4.3).signum())
            .collect();
        assert!(across.is_empty(), "nothing may cross the wall: {across:?}");
    }

    /// Both switched off is both switched off — the level bakes exactly as it
    /// did before any of this existed.
    #[test]
    fn zero_means_off() {
        let s = NavSettings { max_drop: 0.0, max_jump: 0.0, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 1.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.0));
        assert!(bake(&tris, &s).unwrap().off_links.is_empty());
    }

    /// A long ledge is a few ways down, not five hundred identical ones.
    #[test]
    fn a_long_ledge_is_spaced_out_rather_than_carpeted() {
        let s = NavSettings { max_drop: 1.5, max_jump: 0.0, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 20.0, 1.0);
        tris.extend(slab(4.0, 0.0, 4.0, 20.0, 0.0));
        let mesh = bake(&tris, &s).unwrap();
        let n = mesh.off_links.len();
        // The ledge is 200 cells long. At 1 m spacing that is twenty-odd ways
        // down — plus a few round the ends, where the floor also stops — and
        // nothing like the 200 an unspaced pass would give.
        assert!(n > 1, "a 20 m ledge should offer more than one way down");
        assert!(n < 40, "{n} links off one ledge is a carpet");
    }

    /// Ground that walking already reaches gets no link: the router pays for
    /// every edge on every query, and a shortcut is not what was missing.
    #[test]
    fn ground_you_can_already_walk_to_is_not_linked() {
        let s = NavSettings { max_drop: 1.5, max_jump: 1.5, ..tester() };
        // One flat floor with a slot cut most of the way across — the two halves
        // are joined round the end of the slot.
        let mut tris = slab(0.0, 0.0, 8.0, 3.0, 0.0);
        tris.extend(slab(0.0, 3.6, 8.0, 3.0, 0.0));
        tris.extend(slab(6.0, 3.0, 2.0, 0.6, 0.0)); // the bridge round the end
        let mesh = bake(&tris, &s).unwrap();
        assert!(
            mesh.off_links.is_empty(),
            "the two sides are one region already: {:?}",
            mesh.off_links.iter().map(|l| l.name.clone()).collect::<Vec<_>>()
        );
    }

    /// A character that does not jump does not take the jumps, and says so in
    /// the query rather than by needing its own bake.
    #[test]
    fn a_character_that_refuses_a_kind_of_crossing_routes_without_it() {
        use crate::QueryFilter;
        let s = NavSettings { max_drop: 1.5, max_jump: 0.0, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 1.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.0));
        let mesh = bake(&tris, &s).unwrap();
        let (from, to) = ([1.0, 1.0, 2.0], [7.0, 0.0, 2.0]);

        let walker = mesh.path(from, to).unwrap();
        assert!(walker.complete, "a person takes the drop");

        let cart = QueryFilter::default().refusing(LinkKind::Drop);
        let rolled = mesh.path_with(from, to, s.agent_height, &cart).unwrap();
        assert!(!rolled.complete, "a cart does not drop off a ledge");
        assert!(rolled.crossings.is_empty());
    }

    /// A hole a designer carved is not a chasm to hop over. A bake that
    /// invented a way across one would be a tool overruling the person using it.
    #[test]
    fn a_carved_strip_is_never_jumped_across() {
        use crate::AreaVolume;
        let s = NavSettings { max_drop: 1.5, max_jump: 3.0, ..tester() };
        let floor = slab(0.0, 0.0, 12.0, 6.0, 0.0);
        // A 1 m strip across the middle, carved out. The volume is the unit cube
        // in its own frame, so the inverse scales world units down into it.
        let carve = AreaVolume {
            inverse: [
                1.0 / 0.5, 0.0, 0.0, 0.0, //
                0.0, 1.0 / 4.0, 0.0, 0.0, //
                0.0, 0.0, 1.0 / 4.0, 0.0, //
                -6.0 / 0.5, 0.0, -3.0 / 4.0, 1.0,
            ],
            area: crate::WALKABLE,
            blocks: true,
        };
        let mesh = crate::bake_with(&floor, &s, &[carve], Vec::new()).unwrap();
        assert!(
            mesh.polys.len() > 1 && mesh.off_links.is_empty(),
            "the carve made a hole and nothing may bridge it: {:?}",
            mesh.off_links.iter().map(|l| l.name.clone()).collect::<Vec<_>>()
        );
        assert!(!mesh.reachable([2.0, 0.0, 3.0], [10.0, 0.0, 3.0], 1.0));
    }

    /// A jump is two-way, so a chasm is one crossing and not two — the far bank
    /// looking back at us is the link we just made.
    #[test]
    fn a_chasm_is_hopped_once_and_not_from_both_banks() {
        let s = NavSettings { max_drop: 0.0, max_jump: 1.2, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 6.0, 0.0);
        tris.extend(slab(4.6, 0.0, 4.0, 6.0, 0.0));
        let mesh = bake(&tris, &s).unwrap();
        let n = mesh.off_links.len();
        assert!(n > 0, "a 0.6 m gap must be jumped at all");
        // 6 m of bank at 1 m spacing is about six hops. Twelve would mean every
        // one of them exists twice, which is what the router would pay for.
        assert!(n <= 8, "{n} hops across one 6 m chasm is both banks' worth");
    }

    /// Two links answering to one handle is a door that opens something else.
    /// A placed link is somebody's decision, so the generated one gives way.
    #[test]
    fn a_generated_link_never_shadows_a_placed_ones_id_or_name() {
        let s = NavSettings { max_drop: 1.5, max_jump: 0.0, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 1.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.0));
        // Both traps at once: an id at the very top, where counting up from it
        // saturates instead of moving, and the name the generator would pick.
        let mut placed = OffLink::new(u32::MAX, "drop 1", [1.0, 1.0, 1.0], [5.0, 0.0, 1.0]);
        placed.bidirectional = true;
        let mesh = crate::bake_with(&tris, &s, &[], vec![placed]).unwrap();
        assert!(mesh.off_links.len() > 2, "there should be several drops here");

        let mut ids: Vec<u32> = mesh.off_links.iter().map(|l| l.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "every link needs an id of its own");

        let mut names: Vec<String> =
            mesh.off_links.iter().map(|l| l.name.to_ascii_lowercase()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n, "and a name of its own: {names:?}");

        // The placed one keeps both of its own.
        let mine = mesh.off_links.iter().find(|l| l.id == u32::MAX).expect("it is still there");
        assert_eq!(mine.name, "drop 1");
        assert_eq!(mine.kind, LinkKind::Placed);
    }

    /// Generated ids start past the hand-placed ones. Two links a script cannot
    /// tell apart is the failure this avoids.
    #[test]
    fn generated_ids_never_collide_with_placed_ones() {
        let s = NavSettings { max_drop: 1.5, max_jump: 0.0, ..tester() };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 1.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.0));
        let placed = OffLink::new(9, "ladder", [1.0, 1.0, 1.0], [5.0, 0.0, 1.0]);
        let mesh = crate::bake_with(&tris, &s, &[], vec![placed]).unwrap();
        let mut ids: Vec<u32> = mesh.off_links.iter().map(|l| l.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "every link needs its own id");
        assert!(mesh.off_links.iter().any(|l| l.kind == LinkKind::Placed));
        assert!(mesh.off_links.iter().any(|l| l.kind == LinkKind::Drop));
    }
}
