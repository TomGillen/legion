use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Debug,
    hash::BuildHasherDefault,
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    hash::U64Hasher,
    storage::{archetype::ArchetypeIndex, ComponentIndex},
};

/// An opaque identifier for an entity.
#[derive(Debug, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Entity(NonZeroU64);

thread_local! {
    pub static ID_CLONE_MAPPINGS: RefCell<HashMap<Entity, Entity, EntityHasher>> = RefCell::new(HashMap::default());
}

impl Clone for Entity {
    fn clone(&self) -> Self {
        ID_CLONE_MAPPINGS.with(|cell| {
            let map = cell.borrow();
            *map.get(self).unwrap_or(self)
        })
    }
}

const BLOCK_SIZE: u64 = 16;
const BLOCK_SIZE_USIZE: usize = BLOCK_SIZE as usize;

// Always divisible by BLOCK_SIZE.
// Safety: This must never be 0, so skip the first block
static NEXT_ENTITY: AtomicU64 = AtomicU64::new(BLOCK_SIZE);

/// An iterator which yields new entity IDs.
#[derive(Debug)]
pub struct Allocate {
    next: u64,
}

impl Allocate {
    /// Constructs a new enity ID allocator iterator.
    pub fn new() -> Self {
        // This is still safe because the allocator grabs a new block immediately
        Self { next: 0 }
    }
}

impl Default for Allocate {
    fn default() -> Self {
        Self::new()
    }
}

impl Iterator for Allocate {
    type Item = Entity;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.next % BLOCK_SIZE == 0 {
            // This is either the first block, or we overflowed to the next block.
            self.next = NEXT_ENTITY.fetch_add(BLOCK_SIZE, Ordering::Relaxed);
            debug_assert_eq!(self.next % BLOCK_SIZE, 0);
        }

        // Safety: self.next can't be 0 as long as the first block is skipped,
        // and no overflow occurs in NEXT_ENTITY
        let entity = unsafe {
            debug_assert_ne!(self.next, 0);
            Entity(NonZeroU64::new_unchecked(self.next))
        };
        self.next += 1;
        Some(entity)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        // `next()` always returns `Some`,
        // thus naturally it's an endless iterator.
        (usize::MAX, None)
    }
}

/// The storage location of an entity's data.
#[derive(Debug, Copy, Clone)]
pub struct EntityLocation(pub(crate) ArchetypeIndex, pub(crate) ComponentIndex);

impl EntityLocation {
    /// Constructs a new entity location.
    pub fn new(archetype: ArchetypeIndex, component: ComponentIndex) -> Self {
        EntityLocation(archetype, component)
    }

    /// Returns the entity's archetype index.
    pub fn archetype(&self) -> ArchetypeIndex {
        self.0
    }

    /// Returns the entity's component index within its archetype.
    pub fn component(&self) -> ComponentIndex {
        self.1
    }
}

/// A hasher optimized for entity IDs.
pub type EntityHasher = BuildHasherDefault<U64Hasher>;

/// A map of entity IDs to their storage locations.
#[derive(Clone, Default)]
pub struct LocationMap {
    len: usize,
    blocks: HashMap<u64, Box<[Option<EntityLocation>; BLOCK_SIZE_USIZE]>, EntityHasher>,
}

impl Debug for LocationMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.blocks.iter().flat_map(|(base, locs)| {
            locs.iter().enumerate().filter_map(move |(i, loc)| {
                // Safety: as long as the inserted entities are valid, this should also be valid
                let entity = unsafe {
                    let id = *base + i as u64;
                    debug_assert_ne!(id, 0);
                    Entity(NonZeroU64::new_unchecked(id))
                };
                loc.map(|loc| (entity, loc))
            })
        });
        f.debug_map().entries(entries).finish()
    }
}

impl LocationMap {
    /// Returns the number of entities in the map.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the location map is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if the location map contains the given entity.
    pub fn contains(&self, entity: Entity) -> bool {
        self.get(entity).is_some()
    }

    /// Inserts a collection of adjacent entities into the location map.
    pub fn insert(
        &mut self,
        ids: &[Entity],
        arch: ArchetypeIndex,
        ComponentIndex(base): ComponentIndex,
    ) -> Vec<EntityLocation> {
        let mut current_block = u64::MAX;
        let mut block_vec = None;
        let mut removed = Vec::new();
        for (i, entity) in ids.iter().enumerate() {
            let block = entity.0.get() / BLOCK_SIZE;
            if current_block != block {
                block_vec = Some(
                    self.blocks
                        .entry(block)
                        .or_insert_with(|| Box::new([None; BLOCK_SIZE_USIZE])),
                );
                current_block = block;
            }

            if let Some(ref mut vec) = block_vec {
                let idx = (entity.0.get() % BLOCK_SIZE) as usize;
                let loc = EntityLocation(arch, ComponentIndex(base + i));
                if let Some(previous) = vec[idx].replace(loc) {
                    removed.push(previous);
                }
            }
        }

        self.len += ids.len() - removed.len();

        removed
    }

    /// Inserts or updates the location of an entity.
    pub fn set(&mut self, entity: Entity, location: EntityLocation) {
        self.insert(&[entity], location.archetype(), location.component());
    }

    /// Returns the location of an entity.
    pub fn get(&self, entity: Entity) -> Option<EntityLocation> {
        let block = entity.0.get() / BLOCK_SIZE;
        let idx = (entity.0.get() % BLOCK_SIZE) as usize;
        if let Some(&result) = self.blocks.get(&block).and_then(|v| v.get(idx)) {
            result
        } else {
            None
        }
    }

    /// Removes an entity from the location map.
    pub fn remove(&mut self, entity: Entity) -> Option<EntityLocation> {
        use std::collections::hash_map::Entry;

        let block = entity.0.get() / BLOCK_SIZE;
        let idx = (entity.0.get() % BLOCK_SIZE) as usize;

        let r = if let Entry::Occupied(mut block_entry) = self.blocks.entry(block) {
            let block_sloice = block_entry.get_mut();

            let removed = block_sloice.get_mut(idx).and_then(Option::take);

            if removed.is_some() && block_sloice.iter().all(Option::is_none) {
                block_entry.remove();
            }

            removed
        } else {
            None
        };

        if r.is_some() {
            self.len -= 1;
        }

        r
    }
}
