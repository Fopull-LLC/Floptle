//! **A path across a grid of squares** — A* over a tilemap's own cells, for a
//! top-down game whose walls are its solid tiles.
//!
//! A navmesh is the answer for a level of arbitrary geometry; a tile level is
//! already a grid, and the grid is the exact walkable map. Diagonal steps never
//! cut a corner: both squares beside the diagonal must be open, or a character
//! following the path would clip the wall it is drawn walking past.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// What a path search may cross: `None` is a wall, `Some(c)` an open square
/// costing `c` times a plain step (1.0 for ordinary floor).
pub type Cost<'a> = &'a dyn Fn(u32, u32) -> Option<f32>;

/// The squares from `from` to `to`, both included, and whether it reaches
/// `to`. When it cannot, the path goes to the reachable square nearest `to`
/// instead, the way a character walks as close as it can get. `None` when
/// `from` is off the grid or a wall.
pub fn grid_path(
    cols: u32,
    rows: u32,
    cost: Cost,
    from: (u32, u32),
    to: (u32, u32),
    diagonal: bool,
) -> Option<(Vec<(u32, u32)>, bool)> {
    if from.0 >= cols || from.1 >= rows || cost(from.0, from.1).is_none() {
        return None;
    }
    let n = (cols * rows) as usize;
    let idx = |x: u32, y: u32| (y * cols + x) as usize;
    let h = |x: u32, y: u32| {
        let dx = (x as f32 - to.0 as f32).abs();
        let dy = (y as f32 - to.1 as f32).abs();
        if diagonal {
            dx.max(dy) + (std::f32::consts::SQRT_2 - 1.0) * dx.min(dy)
        } else {
            dx + dy
        }
    };
    let mut g = vec![f32::INFINITY; n];
    let mut came = vec![u32::MAX; n];
    let mut closed = vec![false; n];
    // Costs ride the heap as fixed point so it can order them.
    let key = |f: f32| (f * 1024.0) as u64;
    let mut open = BinaryHeap::new();
    g[idx(from.0, from.1)] = 0.0;
    open.push(Reverse((key(h(from.0, from.1)), idx(from.0, from.1) as u32)));
    let mut best = (h(from.0, from.1), idx(from.0, from.1));
    let goal = (to.0 < cols && to.1 < rows).then(|| idx(to.0, to.1));
    const STEPS: [(i32, i32); 8] = [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)];
    while let Some(Reverse((_, i))) = open.pop() {
        let i = i as usize;
        if closed[i] {
            continue;
        }
        closed[i] = true;
        if Some(i) == goal {
            best = (0.0, i);
            break;
        }
        let (x, y) = ((i as u32) % cols, (i as u32) / cols);
        let hx = h(x, y);
        if hx < best.0 {
            best = (hx, i);
        }
        let open_at = |x: i32, y: i32| -> Option<f32> {
            if x < 0 || y < 0 || x >= cols as i32 || y >= rows as i32 {
                return None;
            }
            cost(x as u32, y as u32)
        };
        for &(dx, dy) in &STEPS[..if diagonal { 8 } else { 4 }] {
            let (nx, ny) = (x as i32 + dx, y as i32 + dy);
            let Some(c) = open_at(nx, ny) else { continue };
            if dx != 0 && dy != 0 && (open_at(x as i32 + dx, y as i32).is_none() || open_at(x as i32, y as i32 + dy).is_none()) {
                continue;
            }
            let step = if dx != 0 && dy != 0 { std::f32::consts::SQRT_2 } else { 1.0 };
            let j = idx(nx as u32, ny as u32);
            let ng = g[i] + step * c.max(0.01);
            if ng < g[j] {
                g[j] = ng;
                came[j] = i as u32;
                open.push(Reverse((key(ng + h(nx as u32, ny as u32)), j as u32)));
            }
        }
    }
    let end = best.1;
    let mut out = vec![((end as u32) % cols, (end as u32) / cols)];
    let mut at = end;
    while came[at] != u32::MAX {
        at = came[at] as usize;
        out.push(((at as u32) % cols, (at as u32) / cols));
    }
    out.reverse();
    Some((out, Some(end) == goal))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#` is a wall, `~` costs 5, anything else is floor.
    fn grid<'a>(rows: &'a [&'a str]) -> (u32, u32, impl Fn(u32, u32) -> Option<f32> + 'a) {
        let cols = rows[0].len() as u32;
        (cols, rows.len() as u32, move |x: u32, y: u32| match rows[y as usize].as_bytes()[x as usize] {
            b'#' => None,
            b'~' => Some(5.0),
            _ => Some(1.0),
        })
    }

    #[test]
    fn it_goes_round_a_wall() {
        let (c, r, f) = grid(&["....", ".##.", "...."]);
        let (p, reached) = grid_path(c, r, &f, (0, 1), (3, 1), false).unwrap();
        assert!(reached);
        assert_eq!(p.first(), Some(&(0, 1)));
        assert_eq!(p.last(), Some(&(3, 1)));
        assert_eq!(p.len(), 6, "up or down one, across three, back one: {p:?}");
        assert!(p.iter().all(|&(x, y)| f(x, y).is_some()), "never through the wall");
    }

    #[test]
    fn a_diagonal_never_cuts_a_corner() {
        let (c, r, f) = grid(&[".#", ".."]);
        let (p, _) = grid_path(c, r, &f, (0, 0), (1, 1), true).unwrap();
        assert_eq!(p, vec![(0, 0), (0, 1), (1, 1)], "the wall at (1,0) blocks the diagonal");
        let (c, r, f) = grid(&["..", ".."]);
        let (p, _) = grid_path(c, r, &f, (0, 0), (1, 1), true).unwrap();
        assert_eq!(p, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn a_costly_square_is_walked_round_when_that_is_cheaper() {
        let (c, r, f) = grid(&["...", ".~.", "..."]);
        let (p, _) = grid_path(c, r, &f, (0, 1), (2, 1), false).unwrap();
        assert!(!p.contains(&(1, 1)), "water costs 5; round it costs 2 more steps: {p:?}");
    }

    #[test]
    fn an_unreachable_goal_gets_as_close_as_it_can() {
        let (c, r, f) = grid(&["..#.", "..#."]);
        let (p, reached) = grid_path(c, r, &f, (0, 0), (3, 0), true).unwrap();
        assert!(!reached);
        assert_eq!(p.last().map(|q| q.0), Some(1), "stops beside the wall: {p:?}");
        assert!(grid_path(c, r, &f, (2, 0), (0, 0), true).is_none(), "starting in a wall has no path");
    }
}
