//! Bounded module-query cache keyed on path, source hash, and query shape.
//!
//! Position tools never cache whole-file info trees. Entries are already
//! bounded worker outcomes. Batched proof-agent queries also keep an exact
//! host-side cache because the MCP host opens short-lived worker sessions for
//! each request, while the Lean-side snapshot cache is scoped to one session.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerModuleCacheStatus, LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchOutcome,
    LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector, LeanWorkerModuleQueryTimings, LeanWorkerOutputBudgets,
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

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) struct ModuleQueryBatchKey {
    encoded: String,
}

impl ModuleQueryBatchKey {
    pub(crate) fn from_batch(selectors: &[LeanWorkerModuleQuerySelector], budgets: &LeanWorkerOutputBudgets) -> Self {
        let encoded =
            serde_json::to_string(&(selectors, budgets)).unwrap_or_else(|err| format!("serialization_error:{err}"));
        Self { encoded }
    }
}

#[derive(Debug)]
pub(crate) struct ModuleQueryCache {
    single: Mutex<LruCache<CacheKey, LeanWorkerModuleQueryOutcome>>,
    batch: Mutex<LruCache<BatchCacheKey, LeanWorkerModuleQueryBatchOutcome>>,
}

impl ModuleQueryCache {
    pub(crate) fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            single: Mutex::new(LruCache::new(cap)),
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
        self.single.lock().get(&key).cloned()
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
        self.single.lock().put(key, value);
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
        self.batch.lock().get(&key).cloned().map(mark_batch_cache_hit)
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

fn mark_batch_cache_hit(mut outcome: LeanWorkerModuleQueryBatchOutcome) -> LeanWorkerModuleQueryBatchOutcome {
    let facts = match &mut outcome {
        LeanWorkerModuleQueryBatchOutcome::Ok { facts, .. }
        | LeanWorkerModuleQueryBatchOutcome::MissingImports { facts, .. }
        | LeanWorkerModuleQueryBatchOutcome::HeaderParseFailed { facts, .. } => facts,
        LeanWorkerModuleQueryBatchOutcome::Unsupported => return outcome,
        _ => return outcome,
    };
    facts.cache_status = LeanWorkerModuleCacheStatus::Hit;
    facts.timings = LeanWorkerModuleQueryTimings::zero();
    outcome
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
    use lean_rs_worker_parent::{
        LeanWorkerModuleQuery, LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets, LeanWorkerProofPositionSelector,
    };

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
    fn batch_query_keys_distinguish_selector_payloads() {
        let budgets = LeanWorkerOutputBudgets::default();
        assert_ne!(
            ModuleQueryBatchKey::from_batch(
                &[LeanWorkerModuleQuerySelector::ProofStateInDeclaration {
                    id: "proof_state".into(),
                    declaration: "A.one".into(),
                    position: LeanWorkerProofPositionSelector::default(),
                }],
                &budgets
            ),
            ModuleQueryBatchKey::from_batch(
                &[LeanWorkerModuleQuerySelector::ProofStateInDeclaration {
                    id: "proof_state".into(),
                    declaration: "A.two".into(),
                    position: LeanWorkerProofPositionSelector::default(),
                }],
                &budgets
            )
        );
    }
}
