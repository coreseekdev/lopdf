//! Dense object-number indexed storage for a document's indirect objects.
//!
//! PDF object numbers are small, dense integers.  A `BTreeMap` allocates a
//! node for every object and pays O(log n) on every lookup.  The store keeps a
//! flat `Vec` indexed by object number, with the generation stored beside the
//! value.  Iteration stays ordered by `ObjectId`, matching the old map.

use std::ops::Index;

use crate::object::{Object, ObjectId};

/// A flat, object-number addressed map.
#[derive(Debug, Clone, Default)]
pub struct ObjectStore {
    entries: Vec<Option<(ObjectId, Object)>>,
    len: usize,
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

    pub fn get(&self, id: &ObjectId) -> Option<&Object> {
        let entry = self.entries.get(id.0 as usize)?.as_ref()?;
        (entry.0 == *id).then_some(&entry.1)
    }

    pub fn get_mut(&mut self, id: &ObjectId) -> Option<&mut Object> {
        let entry = self.entries.get_mut(id.0 as usize)?.as_mut()?;
        (entry.0 == *id).then_some(&mut entry.1)
    }

    pub fn contains_key(&self, id: &ObjectId) -> bool {
        self.get(id).is_some()
    }

    pub fn insert(&mut self, id: ObjectId, object: Object) -> Option<Object> {
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
            .filter_map(|entry| entry.as_ref().map(|(id, object)| (id, object)))
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = (&ObjectId, &mut Object)> {
        self.entries
            .iter_mut()
            .filter_map(|entry| entry.as_mut().map(|(id, object)| (&*id, object)))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &ObjectId> {
        self.entries
            .iter()
            .filter_map(|entry| entry.as_ref().map(|(id, _)| id))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Object> {
        self.entries
            .iter()
            .filter_map(|entry| entry.as_ref().map(|(_, object)| object))
    }

    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Object> {
        self.entries
            .iter_mut()
            .filter_map(|entry| entry.as_mut().map(|(_, object)| object))
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
    type IntoIter = std::iter::FilterMap<
        std::slice::Iter<'a, Option<(ObjectId, Object)>>,
        fn(&'a Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a Object)>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn entry<'a>(
            entry: &'a Option<(ObjectId, Object)>,
        ) -> Option<(&'a ObjectId, &'a Object)> {
            entry.as_ref().map(|(id, object)| (id, object))
        }
        self.entries.iter().filter_map(entry)
    }
}

impl<'a> IntoIterator for &'a mut ObjectStore {
    type Item = (&'a ObjectId, &'a mut Object);
    type IntoIter = std::iter::FilterMap<
        std::slice::IterMut<'a, Option<(ObjectId, Object)>>,
        fn(&'a mut Option<(ObjectId, Object)>) -> Option<(&'a ObjectId, &'a mut Object)>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn entry<'a>(
            entry: &'a mut Option<(ObjectId, Object)>,
        ) -> Option<(&'a ObjectId, &'a mut Object)> {
            entry.as_mut().map(|(id, object)| (&*id, object))
        }
        self.entries.iter_mut().filter_map(entry)
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
