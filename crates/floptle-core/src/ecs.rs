//! The data-oriented runtime under everything (ADR-0005).
//!
//! The eventual representation is an **archetype ECS** (entities grouped by exact
//! component set, components stored in packed parallel arrays systems iterate
//! linearly). This module is the **public seam** for that — `Entity`, `World`,
//! `spawn`/`insert`/`get`/`query` — backed for now by a simple per-component store
//! so the rest of Phase 1 has a real, testable world to build on. The archetype
//! packing is an internal swap behind this API; callers won't change.
//!
//! The friendly **Node tree** (ADR-0005, `docs/subsystems/scene-and-nodes.md`) is
//! a facade over this and arrives with the scene module in a later phase.

use std::any::{Any, TypeId};
use std::collections::HashMap;

/// A handle to an entity: a slot index plus a generation that invalidates stale
/// handles after the slot is reused (so a dangling `Entity` can't alias a new one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity {
    index: u32,
    generation: u32,
}

impl Entity {
    pub fn index(self) -> u32 {
        self.index
    }
    pub fn generation(self) -> u32 {
        self.generation
    }
}

/// Type-erased per-component storage. The concrete `Column<T>` lives behind this
/// so `World` can hold a `HashMap<TypeId, Box<dyn AnyColumn>>`.
trait AnyColumn: Any {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    /// Drop any row owned by `e` (called on despawn / component remove).
    fn remove_entity(&mut self, e: Entity);
}

/// Dense, unordered rows of `(owner, component)`. O(n) point lookup today; the
/// archetype rewrite makes it O(1) without touching the `World` API.
struct Column<T> {
    rows: Vec<(Entity, T)>,
}

impl<T> Column<T> {
    fn new() -> Self {
        Self { rows: Vec::new() }
    }
    fn position(&self, e: Entity) -> Option<usize> {
        self.rows.iter().position(|(re, _)| re.index == e.index)
    }
}

impl<T: 'static> AnyColumn for Column<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn remove_entity(&mut self, e: Entity) {
        if let Some(i) = self.position(e) {
            self.rows.swap_remove(i);
        }
    }
}

/// The one source of truth: entities and their components.
#[derive(Default)]
pub struct World {
    generations: Vec<u32>,
    free: Vec<u32>,
    alive: Vec<bool>,
    columns: HashMap<TypeId, Box<dyn AnyColumn>>,
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live entities.
    pub fn len(&self) -> usize {
        self.alive.iter().filter(|a| **a).count()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Create a new entity (no components yet).
    pub fn spawn(&mut self) -> Entity {
        if let Some(index) = self.free.pop() {
            self.alive[index as usize] = true;
            Entity { index, generation: self.generations[index as usize] }
        } else {
            let index = self.generations.len() as u32;
            self.generations.push(0);
            self.alive.push(true);
            Entity { index, generation: 0 }
        }
    }

    /// True if the handle still refers to a live entity.
    pub fn is_alive(&self, e: Entity) -> bool {
        let i = e.index as usize;
        i < self.alive.len() && self.alive[i] && self.generations[i] == e.generation
    }

    /// Destroy an entity and all of its components. Stale handles to it become invalid.
    pub fn despawn(&mut self, e: Entity) {
        if !self.is_alive(e) {
            return;
        }
        for col in self.columns.values_mut() {
            col.remove_entity(e);
        }
        let i = e.index as usize;
        self.alive[i] = false;
        self.generations[i] = self.generations[i].wrapping_add(1);
        self.free.push(e.index);
    }

    fn column<T: 'static>(&self) -> Option<&Column<T>> {
        self.columns.get(&TypeId::of::<T>()).and_then(|c| c.as_any().downcast_ref::<Column<T>>())
    }
    fn column_mut<T: 'static>(&mut self) -> &mut Column<T> {
        self.columns
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(Column::<T>::new()))
            .as_any_mut()
            .downcast_mut::<Column<T>>()
            .expect("column type matches TypeId key")
    }

    /// Attach (or replace) a component on a live entity.
    pub fn insert<T: 'static>(&mut self, e: Entity, value: T) {
        if !self.is_alive(e) {
            return;
        }
        let col = self.column_mut::<T>();
        if let Some(i) = col.position(e) {
            col.rows[i] = (e, value);
        } else {
            col.rows.push((e, value));
        }
    }

    /// Remove a component, returning it if present.
    pub fn remove<T: 'static>(&mut self, e: Entity) -> Option<T> {
        let col = self.column_mut::<T>();
        col.position(e).map(|i| col.rows.swap_remove(i).1)
    }

    pub fn get<T: 'static>(&self, e: Entity) -> Option<&T> {
        let col = self.column::<T>()?;
        col.position(e).map(|i| &col.rows[i].1)
    }
    pub fn get_mut<T: 'static>(&mut self, e: Entity) -> Option<&mut T> {
        let col = self.column_mut::<T>();
        col.position(e).map(|i| &mut col.rows[i].1)
    }

    /// Iterate every `(entity, &T)`. The archetype rewrite makes this a linear
    /// scan over packed arrays; the signature stays.
    pub fn query<T: 'static>(&self) -> impl Iterator<Item = (Entity, &T)> {
        self.column::<T>().into_iter().flat_map(|c| c.rows.iter().map(|(e, v)| (*e, v)))
    }
    /// Mutable iteration over a single component type.
    pub fn query_mut<T: 'static>(&mut self) -> impl Iterator<Item = (Entity, &mut T)> {
        self.column_mut::<T>().rows.iter_mut().map(|(e, v)| (*e, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Name(&'static str);
    #[derive(Debug, PartialEq)]
    struct Hp(i32);

    #[test]
    fn spawn_insert_get() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Name("player"));
        w.insert(e, Hp(100));
        assert_eq!(w.get::<Name>(e), Some(&Name("player")));
        assert_eq!(w.get::<Hp>(e), Some(&Hp(100)));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn despawn_invalidates_and_recycles() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Hp(1));
        w.despawn(e);
        assert!(!w.is_alive(e));
        assert_eq!(w.get::<Hp>(e), None);
        // slot recycles with a bumped generation -> old handle stays invalid
        let e2 = w.spawn();
        assert_eq!(e2.index(), e.index());
        assert_ne!(e2.generation(), e.generation());
        assert!(!w.is_alive(e));
        assert!(w.is_alive(e2));
    }

    #[test]
    fn query_iterates_and_mutates() {
        let mut w = World::new();
        for i in 0..3 {
            let e = w.spawn();
            w.insert(e, Hp(i));
        }
        let total: i32 = w.query::<Hp>().map(|(_, h)| h.0).sum();
        assert_eq!(total, 1 + 2);
        for (_, h) in w.query_mut::<Hp>() {
            h.0 += 10;
        }
        let total: i32 = w.query::<Hp>().map(|(_, h)| h.0).sum();
        assert_eq!(total, 33);
    }
}
