//! Object-number indexed storage for a document's indirect objects.
//!
//! PDF object numbers are normally small and dense, so a flat `Vec` indexed by
//! object number is the fast path: O(1) lookup with no per-node allocation and
//! iteration already in `ObjectId` order.  A document may however carry a very
//! large object number (up to [`crate::MAX_OBJECT_NUMBER`]) while holding only a
//! handful of objects, and sizing the flat `Vec` by that number would allocate
//! gigabytes for nothing.  Numbers at or above [`DENSE_LIMIT`] therefore live
//! in a `BTreeMap` that costs one entry per object, not one slot per id.
//!
//! Both halves keep the same public semantics as the old map: iteration is in
//! ascending `ObjectId` order (dense ids are all below the limit, overflow ids
//! all at or above it), `len` counts objects, and generations stay part of the
//! key comparison.

use std::collections::BTreeMap;
use std::ops::Index;

use crate::object::{Object, ObjectId, MAX_OBJECT_NUMBER};

/// Highest object number that still uses the dense `Vec` fast path.
///
/// One million entries is a few tens of megabytes at worst and covers every
/// real-world document; anything above it takes the sparse path.
pub(crate) const DENSE_LIMIT: u32 = 1 << 20;

/// A flat dense prefix plus a sparse tail, both keyed by `ObjectId`.
#[derive(Debug, Clone, Default)]
pub struct ObjectStore {
    entries: Vec<Option<(ObjectId, Object)>>,
    overflow: BTreeMap<u32, (ObjectId, Object)>,
    len: usize,
}

type DenseRef<'a> = std::iter::FilterMap<
    std::slice::Iter<'a, Option<(ObjectId, Object)>>,
    fn(&'a Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a Object)>,
>;
type OverflowRef<'a> = std::iter::Map<
    std::collections::btree_map::Iter<'a, u32, (ObjectId, Object)>,
    fn((&'a u32, &'a (ObjectId, Object))) -> (&'a ObjectId, &'a Object),
>;
type StoreIter<'a> = std::iter::Chain<DenseRef<'a>, OverflowRef<'a>>;

type DenseMut<'a> = std::iter::FilterMap<
    std::slice::IterMut<'a, Option<(ObjectId, Object)>>,
    fn(&'a mut Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a mut Object)>,
>;
type OverflowMut<'a> = std::iter::Map<
    std::collections::btree_map::IterMut<'a, u32, (ObjectId, Object)>,
    fn((&'a u32, &'a mut (ObjectId, Object))) -> (&'a ObjectId, &'a mut Object),
>;
type StoreIterMut<'a> = std::iter::Chain<DenseMut<'a>, OverflowMut<'a>>;

// The explicit lifetimes are required so these function items coerce to the
// concrete `fn` pointer types in the iterator aliases above.
#[allow(clippy::needless_lifetimes)]
fn dense_ref<'a>(entry: &'a Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a Object)> {
    entry.as_ref().map(|(id, object)| (id, object))
}

#[allow(clippy::needless_lifetimes)]
fn overflow_ref<'a>(pair: (&'a u32, &'a (ObjectId, Object))) -> (&'a ObjectId, &'a Object) {
    let (id, object) = pair.1;
    (id, object)
}

#[allow(clippy::needless_lifetimes)]
fn dense_mut<'a>(
    entry: &'a mut Option<(ObjectId, Object)>,
) -> Option<(&'a ObjectId, &'a mut Object)> {
    entry.as_mut().map(|(id, object)| (&*id, object))
}

#[allow(clippy::needless_lifetimes)]
fn overflow_mut<'a>(
    pair: (&'a u32, &'a mut (ObjectId, Object)),
) -> (&'a ObjectId, &'a mut Object) {
    let (id, object) = pair.1;
    (&*id, object)
}

impl ObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn is_overflow(id: ObjectId) -> bool {
        id.0 >= DENSE_LIMIT
    }

    pub fn get(&self, id: &ObjectId) -> Option<&Object> {
        if Self::is_overflow(*id) {
            let (stored, object) = self.overflow.get(&id.0)?;
            (*stored == *id).then_some(object)
        } else {
            let entry = self.entries.get(id.0 as usize)?.as_ref()?;
            (entry.0 == *id).then_some(&entry.1)
        }
    }

    pub fn get_mut(&mut self, id: &ObjectId) -> Option<&mut Object> {
        if Self::is_overflow(*id) {
            let (stored, object) = self.overflow.get_mut(&id.0)?;
            (*stored == *id).then_some(object)
        } else {
            let entry = self.entries.get_mut(id.0 as usize)?.as_mut()?;
            (entry.0 == *id).then_some(&mut entry.1)
        }
    }

    pub fn contains_key(&self, id: &ObjectId) -> bool {
        self.get(id).is_some()
    }

    pub fn insert(&mut self, id: ObjectId, object: Object) -> Option<Object> {
        assert!(
            id.0 <= MAX_OBJECT_NUMBER,
            "object ID {} exceeds the PDF limit of {}",
            id.0,
            MAX_OBJECT_NUMBER
        );
        if Self::is_overflow(id) {
            let previous = self.overflow.insert(id.0, (id, object));
            if previous.is_none() {
                self.len += 1;
            }
            return previous.and_then(|(previous_id, object)| (previous_id == id).then_some(object));
        }
        let index = id.0 as usize;
        if self.entries.len() <= index {
            self.entries.resize_with(index + 1, || None);
        }
        let slot = &mut self.entries[index];
        let previous = slot.take();
        if previous.is_none() {
            self.len += 1;
        }
        *slot = Some((id, object));
        previous.and_then(|(previous_id, object)| (previous_id == id).then_some(object))
    }

    pub fn remove(&mut self, id: &ObjectId) -> Option<Object> {
        if Self::is_overflow(*id) {
            let slot = self.overflow.remove(&id.0)?;
            if slot.0 != *id {
                // Put a different generation back; the requested id is absent.
                self.overflow.insert(id.0, slot);
                return None;
            }
            self.len -= 1;
            return Some(slot.1);
        }
        let slot = self.entries.get_mut(id.0 as usize)?.take()?;
        if slot.0 != *id {
            // Put a different generation back; the requested id is absent.
            self.entries[id.0 as usize] = Some(slot);
            return None;
        }
        self.len -= 1;
        Some(slot.1)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&ObjectId, &Object)> {
        self.entries
            .iter()
            .filter_map(dense_ref as fn(_) -> _)
            .chain(self.overflow.iter().map(overflow_ref as fn(_) -> _))
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = (&ObjectId, &mut Object)> {
        self.entries
            .iter_mut()
            .filter_map(dense_mut as fn(_) -> _)
            .chain(self.overflow.iter_mut().map(overflow_mut as fn(_) -> _))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &ObjectId> {
        self.iter().map(|(id, _)| id)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Object> {
        self.iter().map(|(_, object)| object)
    }

    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Object> {
        self.iter_mut().map(|(_, object)| object)
    }
}

impl Index<&ObjectId> for ObjectStore {
    type Output = Object;

    fn index(&self, id: &ObjectId) -> &Self::Output {
        self.get(id).expect("object id not present")
    }
}

impl<'a> IntoIterator for &'a ObjectStore {
    type Item = (&'a ObjectId, &'a Object);
    type IntoIter = StoreIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries
            .iter()
            .filter_map(dense_ref as fn(_) -> _)
            .chain(self.overflow.iter().map(overflow_ref as fn(_) -> _))
    }
}

impl<'a> IntoIterator for &'a mut ObjectStore {
    type Item = (&'a ObjectId, &'a mut Object);
    type IntoIter = StoreIterMut<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries
            .iter_mut()
            .filter_map(dense_mut as fn(_) -> _)
            .chain(self.overflow.iter_mut().map(overflow_mut as fn(_) -> _))
    }
}

impl FromIterator<(ObjectId, Object)> for ObjectStore {
    fn from_iter<I: IntoIterator<Item = (ObjectId, Object)>>(iter: I) -> Self {
        let mut store = Self::new();
        for (id, object) in iter {
            store.insert(id, object);
        }
        store
    }
}

#[cfg(feature = "rayon")]
impl rayon::iter::FromParallelIterator<(ObjectId, Object)> for ObjectStore {
    fn from_par_iter<I>(iter: I) -> Self
    where
        I: rayon::iter::IntoParallelIterator<Item = (ObjectId, Object)>,
    {
        use rayon::prelude::*;
        let objects: Vec<(ObjectId, Object)> = iter.into_par_iter().collect();
        objects.into_iter().collect()
    }
}
