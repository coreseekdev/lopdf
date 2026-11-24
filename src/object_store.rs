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
//! An object's generation is part of its key (ISO 32000-1 7.5.4): the same
//! object number may exist at generation 0 in the current cross-reference
//! section and at another generation in an earlier one.  Only generation 0
//! objects use the dense `Vec`; every other generation lives in the sparse map,
//! so inserting `(1, 5)` never replaces `(1, 0)`.
//!
//! Both halves keep the same public semantics as the old map: iteration is in
//! ascending `ObjectId` order (dense ids are all below the limit and at
//! generation 0), `len` counts objects, and generations stay part of the key
//! comparison.

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
    /// Generation 0 objects with a number below [`DENSE_LIMIT`].
    entries: Vec<Option<(ObjectId, Object)>>,
    /// Every other object: a non-zero generation, or a number at or above
    /// [`DENSE_LIMIT`].  Keyed by the whole `ObjectId`, so generations coexist.
    sparse: BTreeMap<ObjectId, Object>,
    len: usize,
}

type DenseRef<'a> = std::iter::FilterMap<
    std::slice::Iter<'a, Option<(ObjectId, Object)>>,
    fn(&'a Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a Object)>,
>;

type DenseMut<'a> = std::iter::FilterMap<
    std::slice::IterMut<'a, Option<(ObjectId, Object)>>,
    fn(&'a mut Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a mut Object)>,
>;

/// Iterator over an [`ObjectStore`] in ascending `ObjectId` order.
///
/// The common case (no sparse entries) is a plain iteration over the dense
/// half.  When sparse entries exist their ids interleave with the dense ones,
/// so the two sorted sequences are merged up front; sparse entries are rare by
/// construction, so this branch costs nothing for ordinary documents.
pub enum ObjectStoreIter<'a> {
    Dense(DenseRef<'a>),
    Merged(std::vec::IntoIter<(&'a ObjectId, &'a Object)>),
}

/// Mutable counterpart of [`ObjectStoreIter`].
pub enum ObjectStoreIterMut<'a> {
    Dense(DenseMut<'a>),
    Merged(std::vec::IntoIter<(&'a ObjectId, &'a mut Object)>),
}

// The explicit lifetimes are required so these function items coerce to the
// concrete `fn` pointer types in the iterator aliases above.
#[allow(clippy::needless_lifetimes)]
fn dense_ref<'a>(entry: &'a Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a Object)> {
    entry.as_ref().map(|(id, object)| (id, object))
}

#[allow(clippy::needless_lifetimes)]
fn dense_mut<'a>(
    entry: &'a mut Option<(ObjectId, Object)>,
) -> Option<(&'a ObjectId, &'a mut Object)> {
    entry.as_mut().map(|(id, object)| (&*id, object))
}

impl<'a> Iterator for ObjectStoreIter<'a> {
    type Item = (&'a ObjectId, &'a Object);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Dense(iter) => iter.next(),
            Self::Merged(iter) => iter.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Dense(iter) => iter.size_hint(),
            Self::Merged(iter) => iter.size_hint(),
        }
    }
}

impl DoubleEndedIterator for ObjectStoreIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Dense(iter) => iter.next_back(),
            Self::Merged(iter) => iter.next_back(),
        }
    }
}

impl<'a> Iterator for ObjectStoreIterMut<'a> {
    type Item = (&'a ObjectId, &'a mut Object);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Dense(iter) => iter.next(),
            Self::Merged(iter) => iter.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Dense(iter) => iter.size_hint(),
            Self::Merged(iter) => iter.size_hint(),
        }
    }
}

impl DoubleEndedIterator for ObjectStoreIterMut<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Dense(iter) => iter.next_back(),
            Self::Merged(iter) => iter.next_back(),
        }
    }
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

    /// Whether `id` belongs in the dense half: generation 0 and a small number.
    #[inline]
    fn is_dense(id: ObjectId) -> bool {
        id.1 == 0 && id.0 < DENSE_LIMIT
    }

    pub fn get(&self, id: &ObjectId) -> Option<&Object> {
        if Self::is_dense(*id) {
            let entry = self.entries.get(id.0 as usize)?.as_ref()?;
            (entry.0 == *id).then_some(&entry.1)
        } else {
            self.sparse.get(id)
        }
    }

    pub fn get_mut(&mut self, id: &ObjectId) -> Option<&mut Object> {
        if Self::is_dense(*id) {
            let entry = self.entries.get_mut(id.0 as usize)?.as_mut()?;
            (entry.0 == *id).then_some(&mut entry.1)
        } else {
            self.sparse.get_mut(id)
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
        if !Self::is_dense(id) {
            return match self.sparse.insert(id, object) {
                Some(previous) => Some(previous),
                None => {
                    self.len += 1;
                    None
                }
            };
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
        if !Self::is_dense(*id) {
            let object = self.sparse.remove(id)?;
            self.len -= 1;
            return Some(object);
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

    /// The dense entries and sparse entries are each sorted by `ObjectId`; when
    /// both are present they are merged so callers still see one ascending
    /// sequence.  Sparse entries are rare, and a document that has any pays a
    /// single `O(n log n)` sort per iteration rather than a per-step merge.
    fn merged_refs(&self) -> Vec<(&ObjectId, &Object)> {
        let mut all: Vec<(&ObjectId, &Object)> = self
            .entries
            .iter()
            .filter_map(dense_ref as fn(_) -> _)
            .chain(self.sparse.iter())
            .collect();
        all.sort_unstable_by_key(|(id, _)| **id);
        all
    }

    pub fn iter(&self) -> ObjectStoreIter<'_> {
        if self.sparse.is_empty() {
            ObjectStoreIter::Dense(self.entries.iter().filter_map(dense_ref as fn(_) -> _))
        } else {
            ObjectStoreIter::Merged(self.merged_refs().into_iter())
        }
    }

    pub fn iter_mut(&mut self) -> ObjectStoreIterMut<'_> {
        if self.sparse.is_empty() {
            ObjectStoreIterMut::Dense(self.entries.iter_mut().filter_map(dense_mut as fn(_) -> _))
        } else {
            let mut all: Vec<(&ObjectId, &mut Object)> = self
                .entries
                .iter_mut()
                .filter_map(dense_mut as fn(_) -> _)
                .chain(self.sparse.iter_mut())
                .collect();
            all.sort_unstable_by_key(|(id, _)| **id);
            ObjectStoreIterMut::Merged(all.into_iter())
        }
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
    type IntoIter = ObjectStoreIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut ObjectStore {
    type Item = (&'a ObjectId, &'a mut Object);
    type IntoIter = ObjectStoreIterMut<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
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
