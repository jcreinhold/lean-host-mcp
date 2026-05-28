//! Bounded module-query cache keyed on path, source hash, and query shape.
//!
//! Position tools never cache whole-file info trees. Entries are already
//! bounded worker outcomes for legacy single-selector projections. Batched
//! proof-agent queries rely on the worker snapshot cache so the host can
//! surface the worker's cache/timing facts honestly.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{LeanWorkerModuleQuery, LeanWorkerModuleQueryOutcome};
use lru::LruCache;
use parking_lot::Mutex;

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct CacheKey {
    file_path: PathBuf,
    content_hash: [u8; 32],
    query: ModuleQueryKey,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) enum ModuleQueryKey {
    Diagnostics,
    TypeAt { line: u32, column: u32 },
    GoalAt { line: u32, column: u32 },
    References { name: String },
    Unknown,
}

impl ModuleQueryKey {
    pub(crate) fn from_query(query: &LeanWorkerModuleQuery) -> Self {
        match query {
            LeanWorkerModuleQuery::Diagnostics => Self::Diagnostics,
            LeanWorkerModuleQuery::TypeAt { line, column } => Self::TypeAt {
                line: *line,
                column: *column,
            },
            LeanWorkerModuleQuery::GoalAt { line, column } => Self::GoalAt {
                line: *line,
                column: *column,
            },
            LeanWorkerModuleQuery::References { name } => Self::References { name: name.clone() },
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ModuleQueryCache {
    inner: Mutex<LruCache<CacheKey, LeanWorkerModuleQueryOutcome>>,
}

impl ModuleQueryCache {
    pub(crate) fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    pub(crate) fn get(
        &self,
        path: &Path,
        content_hash: [u8; 32],
        query: &ModuleQueryKey,
    ) -> Option<LeanWorkerModuleQueryOutcome> {
        let key = CacheKey {
            file_path: path.to_path_buf(),
            content_hash,
            query: query.clone(),
        };
        self.inner.lock().get(&key).cloned()
    }

    pub(crate) fn insert(
        &self,
        path: PathBuf,
        content_hash: [u8; 32],
        query: ModuleQueryKey,
        value: LeanWorkerModuleQueryOutcome,
    ) {
        let key = CacheKey {
            file_path: path,
            content_hash,
            query,
        };
        self.inner.lock().put(key, value);
    }
}

/// SHA-256 the file contents; used to build cache keys without holding the
/// raw source.
pub(crate) fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::ModuleQueryKey;
    use lean_rs_worker_parent::LeanWorkerModuleQuery;

    #[test]
    fn module_query_keys_distinguish_kind_and_payload() {
        assert_ne!(
            ModuleQueryKey::from_query(&LeanWorkerModuleQuery::TypeAt { line: 3, column: 4 }),
            ModuleQueryKey::from_query(&LeanWorkerModuleQuery::GoalAt { line: 3, column: 4 })
        );
        assert_ne!(
            ModuleQueryKey::from_query(&LeanWorkerModuleQuery::References { name: "Nat.add".into() }),
            ModuleQueryKey::from_query(&LeanWorkerModuleQuery::References { name: "Nat.mul".into() })
        );
    }
}
