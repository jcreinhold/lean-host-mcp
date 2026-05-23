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
//! purpose.
//!
//! [`ProcessedFile`] is `Send + Sync + 'static` (owned strings / `u32`s only),
//! so the cached `Arc<ProcessedFile>` traverses tokio task boundaries without
//! going back through the actor.

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

/// Bounded LRU of [`ProcessedFile`] projections keyed on
/// `(file_path, sha256(contents))`. Any edit to the source bytes misses
/// structurally, so stale entries are impossible.
#[derive(Debug)]
pub struct ProcessedFileCache {
    inner: Mutex<LruCache<CacheKey, Arc<ProcessedFile>>>,
}

impl ProcessedFileCache {
    pub fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn get(&self, path: &Path, content_hash: [u8; 32]) -> Option<Arc<ProcessedFile>> {
        let key = CacheKey {
            file_path: path.to_path_buf(),
            content_hash,
        };
        let mut guard = self.inner.lock();
        guard.get(&key).map(Arc::clone)
    }

    pub fn insert(&self, path: PathBuf, content_hash: [u8; 32], value: Arc<ProcessedFile>) {
        let key = CacheKey {
            file_path: path,
            content_hash,
        };
        let mut guard = self.inner.lock();
        guard.put(key, value);
    }
}

/// SHA-256 the file contents — used to build cache keys without holding the
/// raw source.
pub(crate) fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}
