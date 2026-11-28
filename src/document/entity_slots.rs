//! Native ordered entity storage. Only the original Arc<EntityType> is
//! stored in each persistent leaf; no mirrored geometry or double Arc.
//! Preserves the existing flat-slot order and swap-remove semantics used
//! by the DWG/DXF readers, writers and the handle index.

use std::ops::{Index, IndexMut};
use std::sync::Arc;
use super::hy3d_persist::{CowSeq, SeqSharedIter};
use crate::entities::EntityType;

#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize), serde(transparent))]
pub(crate) struct EntitySlots(CowSeq<EntityType>);

impl EntitySlots {
    pub(crate) fn new() -> Self { Self(CowSeq::new()) }
    pub(crate) fn len(&self) -> usize { self.0.len() }
    pub(crate) fn into_arcs(self) -> Vec<Arc<EntityType>> { self.0.into_arcs() }
    pub(crate) fn is_empty(&self) -> bool { self.0.is_empty() }
    pub(crate) fn reserve(&mut self, _additional: usize) {}
    pub(crate) fn clear(&mut self) { self.0.clear(); }
    pub(crate) fn get(&self, index: usize) -> Option<&Arc<EntityType>> {
        self.0.shared_at(index)
    }
    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut Arc<EntityType>> {
        self.0.shared_at_mut(index)
    }
    pub(crate) fn iter(&self) -> SeqSharedIter<'_, EntityType> { self.0.shared_iter() }
    pub(crate) fn iter_mut(&mut self) -> std::vec::IntoIter<&mut Arc<EntityType>> {
        self.0.shared_iter_mut()
    }
    pub(crate) fn push(&mut self, entity: Arc<EntityType>) { self.0.push_shared(entity); }
    pub(crate) fn append(&mut self, entities: &mut Vec<Arc<EntityType>>) {
        if self.is_empty() {
            self.0 = CowSeq::from_shared_vec(std::mem::take(entities));
        } else {
            for entity in entities.drain(..) { self.push(entity); }
        }
    }
    pub(crate) fn swap_remove(&mut self, index: usize) -> Arc<EntityType> {
        self.0.swap_remove_shared(index)
    }
}

impl Index<usize> for EntitySlots {
    type Output = Arc<EntityType>;
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("entity slot out of range")
    }
}

impl IndexMut<usize> for EntitySlots {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.get_mut(index).expect("entity slot out of range")
    }
}

impl<'a> IntoIterator for &'a EntitySlots {
    type Item = &'a Arc<EntityType>;
    type IntoIter = SeqSharedIter<'a, EntityType>;
    fn into_iter(self) -> Self::IntoIter { self.iter() }
}
impl<'a> IntoIterator for &'a mut EntitySlots {
    type Item = &'a mut Arc<EntityType>;
    type IntoIter = std::vec::IntoIter<&'a mut Arc<EntityType>>;
    fn into_iter(self) -> Self::IntoIter { self.iter_mut() }
}
