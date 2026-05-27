//! Bounded module-query cache keyed on path, source hash, and query shape.
//!
//! Position tools never cache whole-file info trees. Entries are already
//! bounded worker outcomes: either one legacy single-selector projection or
//! one exact batch projection.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryOutcome,
    LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets,
};
use lru::LruCache;
use parking_lot::Mutex;

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct CacheKey {
    file_path: PathBuf,
    content_hash: [u8; 32],
    query: ModuleQueryKey,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct BatchCacheKey {
    file_path: PathBuf,
    content_hash: [u8; 32],
    query: ModuleQueryBatchKey,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) enum ModuleQueryKey {
    Diagnostics,
    TypeAt { line: u32, column: u32 },
    GoalAt { line: u32, column: u32 },
    References { name: String },
    Unknown,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) struct ModuleQueryBatchKey {
    selectors: Vec<ModuleQuerySelectorKey>,
    per_field_bytes: u32,
    total_bytes: u32,
}

impl ModuleQueryBatchKey {
    pub(crate) fn from_selectors(
        selectors: &[LeanWorkerModuleQuerySelector],
        budgets: &LeanWorkerOutputBudgets,
    ) -> Self {
        Self {
            selectors: selectors.iter().map(ModuleQuerySelectorKey::from).collect(),
            per_field_bytes: budgets.per_field_bytes,
            total_bytes: budgets.total_bytes,
        }
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
enum ModuleQuerySelectorKey {
    Diagnostics {
        id: String,
    },
    ProofState {
        id: String,
        line: u32,
        column: u32,
    },
    TypeAt {
        id: String,
        line: u32,
        column: u32,
    },
    References {
        id: String,
        name: String,
    },
    DeclarationTarget {
        id: String,
        name: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    },
    SurroundingDeclaration {
        id: String,
        line: u32,
        column: u32,
    },
    Unknown,
}

impl From<&LeanWorkerModuleQuerySelector> for ModuleQuerySelectorKey {
    fn from(selector: &LeanWorkerModuleQuerySelector) -> Self {
        match selector {
            LeanWorkerModuleQuerySelector::Diagnostics { id } => Self::Diagnostics { id: id.clone() },
            LeanWorkerModuleQuerySelector::ProofState { id, line, column } => Self::ProofState {
                id: id.clone(),
                line: *line,
                column: *column,
            },
            LeanWorkerModuleQuerySelector::TypeAt { id, line, column } => Self::TypeAt {
                id: id.clone(),
                line: *line,
                column: *column,
            },
            LeanWorkerModuleQuerySelector::References { id, name } => Self::References {
                id: id.clone(),
                name: name.clone(),
            },
            LeanWorkerModuleQuerySelector::DeclarationTarget { id, name, line, column } => Self::DeclarationTarget {
                id: id.clone(),
                name: name.clone(),
                line: *line,
                column: *column,
            },
            LeanWorkerModuleQuerySelector::SurroundingDeclaration { id, line, column } => {
                Self::SurroundingDeclaration {
                    id: id.clone(),
                    line: *line,
                    column: *column,
                }
            }
            _ => Self::Unknown,
        }
    }
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
    batch: Mutex<LruCache<BatchCacheKey, LeanWorkerModuleQueryBatchOutcome>>,
}

impl ModuleQueryCache {
    pub(crate) fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            batch: Mutex::new(LruCache::new(cap)),
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

    pub(crate) fn get_batch(
        &self,
        path: &Path,
        content_hash: [u8; 32],
        query: &ModuleQueryBatchKey,
    ) -> Option<LeanWorkerModuleQueryBatchOutcome> {
        let key = BatchCacheKey {
            file_path: path.to_path_buf(),
            content_hash,
            query: query.clone(),
        };
        self.batch.lock().get(&key).cloned()
    }

    pub(crate) fn insert_batch(
        &self,
        path: PathBuf,
        content_hash: [u8; 32],
        query: ModuleQueryBatchKey,
        value: LeanWorkerModuleQueryBatchOutcome,
    ) {
        let key = BatchCacheKey {
            file_path: path,
            content_hash,
            query,
        };
        self.batch.lock().put(key, value);
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
    use super::{ModuleQueryBatchKey, ModuleQueryKey};
    use lean_rs_worker_parent::{LeanWorkerModuleQuery, LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets};

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

    #[test]
    fn module_query_batch_keys_distinguish_selector_ids_and_budget() {
        let selectors = vec![LeanWorkerModuleQuerySelector::Diagnostics { id: "d".into() }];
        let same_shape_other_id = vec![LeanWorkerModuleQuerySelector::Diagnostics { id: "other".into() }];
        let default_budget = LeanWorkerOutputBudgets::default();
        let smaller_budget = LeanWorkerOutputBudgets {
            total_bytes: 1024,
            ..default_budget
        };

        assert_ne!(
            ModuleQueryBatchKey::from_selectors(&selectors, &default_budget),
            ModuleQueryBatchKey::from_selectors(&same_shape_other_id, &default_budget)
        );
        assert_ne!(
            ModuleQueryBatchKey::from_selectors(&selectors, &default_budget),
            ModuleQueryBatchKey::from_selectors(&selectors, &smaller_budget)
        );
    }
}
