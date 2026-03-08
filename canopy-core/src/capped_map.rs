//! A size-capped map with FIFO eviction.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// A HashMap with a maximum capacity that evicts the oldest entry on overflow.
///
/// Insertions of existing keys update the value without changing eviction order.
pub struct CappedMap<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: Eq + Hash + Clone, V> CappedMap<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    /// Insert or update a key. Returns `true` if the key was new.
    pub fn insert(&mut self, key: K, value: V) -> bool {
        let is_new = !self.entries.contains_key(&key);
        self.entries.insert(key.clone(), value);
        if is_new {
            self.order.push_back(key);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.entries.remove(&old);
                }
            }
        }
        is_new
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.values()
    }

    /// Retain only entries where the predicate returns true.
    /// Order entries for evicted keys become lazy tombstones.
    pub fn retain<F: FnMut(&K, &mut V) -> bool>(&mut self, f: F) {
        self.entries.retain(f);
    }
}

/// A HashSet with a maximum capacity and FIFO eviction.
pub struct CappedSet<K> {
    inner: CappedMap<K, ()>,
}

impl<K: Eq + Hash + Clone> CappedSet<K> {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: CappedMap::new(cap),
        }
    }

    /// Insert a key. Returns `true` if the key was new.
    pub fn insert(&mut self, key: K) -> bool {
        self.inner.insert(key, ())
    }

    pub fn contains(&self, key: &K) -> bool {
        self.inner.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Retain only keys where the predicate returns true.
    pub fn retain<F: FnMut(&K) -> bool>(&mut self, mut f: F) {
        self.inner.retain(|k, _: &mut ()| f(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_on_overflow() {
        let mut map = CappedMap::new(2);
        map.insert("a", 1);
        map.insert("b", 2);
        map.insert("c", 3);
        assert!(map.get(&"a").is_none());
        assert_eq!(*map.get(&"b").unwrap(), 2);
        assert_eq!(*map.get(&"c").unwrap(), 3);
    }

    #[test]
    fn update_existing_does_not_evict() {
        let mut map = CappedMap::new(2);
        map.insert("a", 1);
        map.insert("b", 2);
        map.insert("a", 10); // update, not new
        assert_eq!(*map.get(&"a").unwrap(), 10);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn capped_set_eviction() {
        let mut set = CappedSet::new(2);
        assert!(set.insert("x"));
        assert!(set.insert("y"));
        assert!(set.insert("z"));
        assert!(!set.contains(&"x"));
        assert!(set.contains(&"y"));
        assert!(set.contains(&"z"));
    }
}
