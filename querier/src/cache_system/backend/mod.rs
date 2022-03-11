use std::{fmt::Debug, hash::Hash};

pub mod hash_map;

#[cfg(test)]
mod test_util;

/// Backend to keep and manage stored entries.
///
/// A backend might remove entries at any point, e.g. due to memory pressure or expiration.
pub trait CacheBackend: Debug + Send + 'static {
    /// Cache key.
    type K: Clone + Eq + Hash + Ord + Debug + Send + 'static;

    /// Cached value.
    type V: Clone + Debug + Send + 'static;

    /// Get value for given key if it exists.
    fn get(&mut self, k: &Self::K) -> Option<Self::V>;

    /// Set value for given key.
    ///
    /// It is OK to set and override a key that already exists.
    fn set(&mut self, k: Self::K, v: Self::V);

    /// Remove value for given key.
    ///
    /// It is OK to remove a key even when it does not exist.
    fn remove(&mut self, k: &Self::K);
}