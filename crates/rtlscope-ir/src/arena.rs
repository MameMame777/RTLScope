//! Typed arena and index.
//!
//! `id-arena` and `la-arena` do not implement `serde`, and RTLScope needs total
//! control over how ids appear in JSON: an [`Idx`] is a bare integer and an
//! [`Arena`] is a bare array, so the MCP schema needs no explanation beyond
//! "this number indexes that array".

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A typed index into an [`Arena<T>`].
///
/// `PhantomData<fn() -> T>` keeps `Idx` covariant in `T` and unconditionally
/// `Send + Sync`; every trait below is implemented by hand because deriving
/// would demand the same bound of `T`.
pub struct Idx<T> {
    raw: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Idx<T> {
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self { raw, _marker: PhantomData }
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.raw
    }

    #[inline]
    pub const fn index(self) -> usize {
        self.raw as usize
    }
}

impl<T> Clone for Idx<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Idx<T> {}

impl<T> PartialEq for Idx<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Idx<T> {}

impl<T> PartialOrd for Idx<T> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Idx<T> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> Hash for Idx<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T> fmt::Debug for Idx<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.raw)
    }
}

impl<T> Serialize for Idx<T> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_u32(self.raw)
    }
}

impl<'de, T> Deserialize<'de> for Idx<T> {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Ok(Self::from_raw(u32::deserialize(de)?))
    }
}

/// A append-only vector addressed by [`Idx<T>`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Arena<T> {
    items: Vec<T>,
}

impl<T> Arena<T> {
    pub const fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { items: Vec::with_capacity(cap) }
    }

    pub fn alloc(&mut self, value: T) -> Idx<T> {
        let raw = u32::try_from(self.items.len()).expect("arena exceeded u32::MAX entries");
        self.items.push(value);
        Idx::from_raw(raw)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[inline]
    pub fn get(&self, idx: Idx<T>) -> Option<&T> {
        self.items.get(idx.index())
    }

    #[inline]
    pub fn get_mut(&mut self, idx: Idx<T>) -> Option<&mut T> {
        self.items.get_mut(idx.index())
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.items.iter_mut()
    }

    /// Iterate `(index, value)` pairs — the usual way to walk an arena when the
    /// ids are needed downstream (graph building, diagnostics).
    pub fn iter_enumerated(&self) -> impl Iterator<Item = (Idx<T>, &T)> {
        self.items.iter().enumerate().map(|(i, v)| (Idx::from_raw(i as u32), v))
    }

    pub fn indices(&self) -> impl Iterator<Item = Idx<T>> + use<T> {
        (0..self.items.len() as u32).map(Idx::from_raw)
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Index<Idx<T>> for Arena<T> {
    type Output = T;
    #[inline]
    fn index(&self, idx: Idx<T>) -> &T {
        &self.items[idx.index()]
    }
}

impl<T> IndexMut<Idx<T>> for Arena<T> {
    #[inline]
    fn index_mut(&mut self, idx: Idx<T>) -> &mut T {
        &mut self.items[idx.index()]
    }
}

impl<T> FromIterator<T> for Arena<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self { items: iter.into_iter().collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Thing(u8);

    #[test]
    fn alloc_returns_sequential_indices() {
        let mut arena = Arena::new();
        let a = arena.alloc(Thing(1));
        let b = arena.alloc(Thing(2));
        assert_eq!(a.raw(), 0);
        assert_eq!(b.raw(), 1);
        assert_eq!(arena[a], Thing(1));
        assert_eq!(arena.len(), 2);
    }

    #[test]
    fn idx_serialises_as_bare_integer() {
        let idx: Idx<Thing> = Idx::from_raw(7);
        assert_eq!(serde_json::to_string(&idx).unwrap(), "7");
        let back: Idx<Thing> = serde_json::from_str("7").unwrap();
        assert_eq!(back, idx);
    }

    #[test]
    fn arena_serialises_as_bare_array() {
        let arena: Arena<Thing> = [Thing(1), Thing(2)].into_iter().collect();
        assert_eq!(serde_json::to_string(&arena).unwrap(), "[1,2]");
        let back: Arena<Thing> = serde_json::from_str("[1,2]").unwrap();
        assert_eq!(back, arena);
    }
}
