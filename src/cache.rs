//! In-memory cache of [`ProcessedFile`] projections keyed on path + content
//! hash.
//!
//! Position-based tools (`goal_at_position`, `type_at_position`,
//! `references_of_name`) want cheap repeat lookups against the same source
//! file — a cursor that moves twenty times per minute should not re-elaborate
//! the file twenty times. The cache lives on
//! [`ToolContext`](crate::tools::ToolContext) and is consulted on every call.
//!
//! Keying invariant: the SHA-256 of the file contents is part of the key, so
//! any edit (even a whitespace change) misses and re-processes. Imports are
//! deliberately *not* in the key — within a server session callers reuse the
//! same `default_imports`, and re-processing on import drift would defeat the
//! purpose. A future variant that takes imports may revisit this.
//!
//! [`ProcessedFile`] is `Send + Sync + 'static` (owned strings / `u32`s only),
//! so the cached `Arc<ProcessedFile>` traverses tokio task boundaries without
//! going back through the actor.
//!
//! The cache is parameterised over the cached value so the LRU + key
//! behaviour can be exercised by unit tests with a stand-in value — the
//! upstream `ProcessedFile` has no public constructor.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lean_rs_host::host::process::ProcessedFile;
use lru::LruCache;
use parking_lot::Mutex;

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct CacheKey {
    file_path: PathBuf,
    content_hash: [u8; 32],
}

/// Bounded LRU keyed on `(path, content_hash)`. Generic over the cached
/// value so tests can stand in a cheap type; production aliases the value
/// to [`ProcessedFile`] via [`ProcessedFileCache`].
#[derive(Debug)]
pub struct FileCache<V> {
    inner: Mutex<LruCache<CacheKey, Arc<V>>>,
}

impl<V> FileCache<V> {
    pub fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn get(&self, path: &Path, content_hash: [u8; 32]) -> Option<Arc<V>> {
        let key = CacheKey {
            file_path: path.to_path_buf(),
            content_hash,
        };
        let mut guard = self.inner.lock();
        guard.get(&key).map(Arc::clone)
    }

    pub fn insert(&self, path: PathBuf, content_hash: [u8; 32], value: Arc<V>) {
        let key = CacheKey {
            file_path: path,
            content_hash,
        };
        let mut guard = self.inner.lock();
        guard.put(key, value);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

/// The production specialisation: an LRU of [`ProcessedFile`] projections.
pub type ProcessedFileCache = FileCache<ProcessedFile>;

/// SHA-256 the file contents — used to build cache keys without holding the
/// raw source.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn hit_returns_cached_value() {
        let cache: FileCache<u32> = FileCache::with_capacity(NonZeroUsize::new(4).unwrap());
        let path = PathBuf::from("/tmp/a.lean");
        let h = hash_bytes(b"source");
        cache.insert(path.clone(), h, Arc::new(7));
        assert_eq!(cache.get(&path, h).as_deref().copied(), Some(7));
    }

    #[test]
    fn miss_on_different_hash() {
        let cache: FileCache<u32> = FileCache::with_capacity(NonZeroUsize::new(4).unwrap());
        let path = PathBuf::from("/tmp/a.lean");
        cache.insert(path.clone(), hash_bytes(b"v1"), Arc::new(1));
        assert!(cache.get(&path, hash_bytes(b"v2")).is_none());
    }

    #[test]
    fn miss_on_different_path() {
        let cache: FileCache<u32> = FileCache::with_capacity(NonZeroUsize::new(4).unwrap());
        let h = hash_bytes(b"v1");
        cache.insert(PathBuf::from("/a"), h, Arc::new(1));
        assert!(cache.get(Path::new("/b"), h).is_none());
    }

    #[test]
    fn evicts_lru_at_capacity() {
        let cap = NonZeroUsize::new(2).unwrap();
        let cache: FileCache<u32> = FileCache::with_capacity(cap);
        let h1 = hash_bytes(b"a");
        let h2 = hash_bytes(b"b");
        let h3 = hash_bytes(b"c");
        cache.insert(PathBuf::from("/a"), h1, Arc::new(1));
        cache.insert(PathBuf::from("/b"), h2, Arc::new(2));
        cache.insert(PathBuf::from("/c"), h3, Arc::new(3));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(Path::new("/a"), h1).is_none(), "oldest entry must be evicted");
        assert!(cache.get(Path::new("/c"), h3).is_some());
    }

    #[test]
    fn hash_distinguishes_inputs() {
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
    }
}
