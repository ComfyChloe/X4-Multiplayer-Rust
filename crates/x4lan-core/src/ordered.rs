//! A tiny insertion-ordered map.
//!
//! The Python reference implementation uses plain `dict` and `OrderedDict` for
//! peer and command tables. CPython dictionaries preserve insertion order, and
//! several parts of the protocol depend on that: the relay emits snapshots in
//! the order peers joined, and a command is dispatched in the order it was
//! accepted. `HashMap` would silently reorder both, so the Rust port needs an
//! explicitly ordered container to stay byte-for-byte compatible.

use std::borrow::Borrow;

/// Insertion-ordered map with the small slice of `HashMap` API the relay needs.
#[derive(Debug, Clone, Default)]
pub struct OrderedMap<K, V> {
    entries: Vec<(K, V)>,
}

impl<K: PartialEq, V> OrderedMap<K, V> {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.entries.iter().any(|(k, _)| k.borrow() == key)
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.entries
            .iter()
            .find(|(k, _)| k.borrow() == key)
            .map(|(_, v)| v)
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.entries
            .iter_mut()
            .find(|(k, _)| k.borrow() == key)
            .map(|(_, v)| v)
    }

    /// Inserts a value, keeping the original position when the key already exists.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            return Some(std::mem::replace(&mut slot.1, value));
        }
        self.entries.push((key, value));
        None
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        let index = self.entries.iter().position(|(k, _)| k.borrow() == key)?;
        Some(self.entries.remove(index).1)
    }

    /// Inserts at the front, which is how the relay recycles the oldest command.
    pub fn insert_front(&mut self, key: K, value: V) {
        self.entries.insert(0, (key, value));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.entries.iter_mut().map(|(k, v)| (&*k, v))
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.entries.iter_mut().map(|(_, v)| v)
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.iter().map(|(k, _)| k)
    }

    /// Removes every entry for which the predicate returns `false`.
    pub fn retain<F: FnMut(&K, &mut V) -> bool>(&mut self, mut keep: F) {
        self.entries.retain_mut(|(k, v)| keep(k, v));
    }
}

impl<K: PartialEq, V> FromIterator<(K, V)> for OrderedMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut map = OrderedMap::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order() {
        let mut map: OrderedMap<&str, i32> = OrderedMap::new();
        map.insert("p1", 1);
        map.insert("p2", 2);
        map.insert("p3", 3);
        let order: Vec<_> = map.keys().copied().collect();
        assert_eq!(order, ["p1", "p2", "p3"]);
    }

    #[test]
    fn reinsertion_keeps_position() {
        let mut map: OrderedMap<&str, i32> = OrderedMap::new();
        map.insert("p1", 1);
        map.insert("p2", 2);
        map.insert("p1", 10);
        let order: Vec<_> = map.keys().copied().collect();
        assert_eq!(order, ["p1", "p2"]);
        assert_eq!(map.get(&"p1"), Some(&10));
    }

    #[test]
    fn removal_preserves_remaining_order() {
        let mut map: OrderedMap<&str, i32> = OrderedMap::new();
        for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
            map.insert(k, v);
        }
        map.remove(&"b");
        let order: Vec<_> = map.keys().copied().collect();
        assert_eq!(order, ["a", "c"]);
    }
}
