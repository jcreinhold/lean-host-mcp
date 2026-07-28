//! Bounded module-query cache keyed on path, source hash, and query shape.
//!
//! Position tools never cache whole-file info trees. Entries are already
//! bounded worker outcomes. Batched proof-agent queries also keep an exact
//! host-side cache because the MCP host opens short-lived worker sessions for
//! each request, while the Lean-side snapshot cache is scoped to one session.
//!
//! **There is no TTL, deliberately.** The source hash *is* the invalidation: an
//! entry can only be served to a caller whose file hashes to the same bytes, so
//! there is no interval after which a hit becomes less true than it was. A TTL
//! would evict correct entries on a timer and still not make a stale one safe.
//!
//! **Only file-keyed module queries are cached**, and the reason is the same
//! one stated positively. `inspect_declaration`, `search_declarations`,
//! `attempt_proof`, and `verify_*` name declarations rather than supply source,
//! so a key for them would have no content hash to invalidate on: the
//! environment those names resolve against changes whenever any `.olean` in the
//! import closure is rebuilt, and the manifest hash the broker tracks pins
//! dependency *revisions*, not local build output. The proof-action tools
//! additionally carry heartbeat budgets, so a cached success would be answering
//! a question about a different elaboration budget than the one asked. Each
//! bypass site carries a one-line pointer back here.

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

/// One query, reduced to the part of it that a cached answer depends on.
///
/// Every variant of [`LeanWorkerModuleQuery`] this repo knows about has its own
/// shape here. There is deliberately **no** catch-all: `LeanWorkerModuleQuery`
/// is `#[non_exhaustive]`, so a variant added upstream would land in a wildcard
/// arm and share one key with every other unrecognized query — different
/// questions about the same file answered from each other's cache entries.
/// [`Self::from_query`] returns `None` instead, and the caller skips the cache.
#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) enum ModuleQueryKey {
    Diagnostics,
    TypeAt { line: u32, column: u32 },
    GoalAt { line: u32, column: u32 },
    References { name: String },
}

impl ModuleQueryKey {
    /// `None` when `query` is a variant this build does not model — see the
    /// type's own documentation. Not cacheable is always safe; the query still
    /// runs, it just runs every time.
    pub(crate) fn from_query(query: &LeanWorkerModuleQuery) -> Option<Self> {
        match query {
            LeanWorkerModuleQuery::Diagnostics => Some(Self::Diagnostics),
            LeanWorkerModuleQuery::TypeAt { line, column } => Some(Self::TypeAt {
                line: *line,
                column: *column,
            }),
            LeanWorkerModuleQuery::GoalAt { line, column } => Some(Self::GoalAt {
                line: *line,
                column: *column,
            }),
            LeanWorkerModuleQuery::References { name } => Some(Self::References { name: name.clone() }),
            _ => None,
        }
    }
}

/// A batch reduced to its selectors and byte budgets, serialized.
///
/// Selectors are an open `#[non_exhaustive]` set, so enumerating them the way
/// [`ModuleQueryKey`] enumerates queries would need an arm per variant *and* a
/// catch-all. Serializing sidesteps that: a variant this build cannot name
/// still encodes to distinct bytes. The budgets are in the key because they
/// bound the *answer*, not the question — a reply truncated to 8 KiB must not
/// be served to a caller who asked for 64 KiB.
#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) struct ModuleQueryBatchKey {
    encoded: String,
}

impl ModuleQueryBatchKey {
    /// `None` when the batch does not serialize, for the same reason
    /// [`ModuleQueryKey::from_query`] returns `None`: the old fallback keyed
    /// on the error *message*, so two different unserializable batches that
    /// failed the same way shared one entry.
    pub(crate) fn from_batch(
        selectors: &[LeanWorkerModuleQuerySelector],
        budgets: &LeanWorkerOutputBudgets,
    ) -> Option<Self> {
        serde_json::to_string(&(selectors, budgets))
            .ok()
            .map(|encoded| Self { encoded })
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
#[allow(
    clippy::unwrap_used,
    reason = "a key these tests construct from a variant they name is always Some; a panic is the honest failure"
)]
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

    /// Every query shape this build ships is keyable, so nothing in the current
    /// tool surface silently falls off the cache. The complementary half —
    /// that an *unrecognized* variant yields `None` rather than a shared key —
    /// cannot be written as a test: `LeanWorkerModuleQuery` is
    /// `#[non_exhaustive]`, so this crate cannot construct a variant it does
    /// not know. That is exactly why the wildcard arm returns `None`: the
    /// unrepresentable case has to be safe by construction, not by assertion.
    #[test]
    fn every_shipped_query_shape_is_keyable() {
        for query in [
            LeanWorkerModuleQuery::Diagnostics,
            LeanWorkerModuleQuery::TypeAt { line: 1, column: 2 },
            LeanWorkerModuleQuery::GoalAt { line: 1, column: 2 },
            LeanWorkerModuleQuery::References { name: "Nat.add".into() },
        ] {
            assert!(
                ModuleQueryKey::from_query(&query).is_some(),
                "{query:?} lost its cache key"
            );
        }
    }

    /// The property the `Unknown` key violated, stated end-to-end through the
    /// cache rather than on key equality alone: one file's answer to one
    /// question is never served as its answer to a different one.
    #[test]
    fn one_files_answer_is_never_served_for_a_different_question() {
        let cache = super::ModuleQueryCache::with_capacity(std::num::NonZeroUsize::new(4).unwrap());
        let path = std::path::Path::new("/demo/Basic.lean");
        let hash = super::hash_bytes(b"theorem t : True := trivial");
        let stored = ModuleQueryKey::from_query(&LeanWorkerModuleQuery::TypeAt { line: 1, column: 2 }).unwrap();

        cache.insert(
            path.to_path_buf(),
            hash,
            stored.clone(),
            lean_rs_worker_parent::LeanWorkerModuleQueryOutcome::Unsupported,
        );

        assert!(cache.get(path, hash, &stored).is_some(), "same question, same answer");
        for other in [
            LeanWorkerModuleQuery::Diagnostics,
            LeanWorkerModuleQuery::GoalAt { line: 1, column: 2 },
            LeanWorkerModuleQuery::TypeAt { line: 9, column: 9 },
            LeanWorkerModuleQuery::References { name: "Nat.add".into() },
        ] {
            let key = ModuleQueryKey::from_query(&other).unwrap();
            assert!(cache.get(path, hash, &key).is_none(), "{other:?} read a foreign entry");
        }
        // Same question, edited file: the content hash is the invalidation, so
        // this must miss without any TTL involved.
        assert!(cache.get(path, super::hash_bytes(b"edited"), &stored).is_none());
    }

    /// Budgets bound the *answer*, not the question, so two batches that differ
    /// only in their byte budgets are different cache entries. Without this a
    /// reply truncated to a small budget would be served to a caller who asked
    /// for a larger one, silently losing content that was never fetched.
    #[test]
    fn batch_query_keys_distinguish_output_budgets() {
        let selectors = [LeanWorkerModuleQuerySelector::Diagnostics {
            id: "diagnostics".into(),
        }];
        assert_ne!(
            ModuleQueryBatchKey::from_batch(
                &selectors,
                &LeanWorkerOutputBudgets {
                    per_field_bytes: 4 * 1024,
                    total_bytes: 64 * 1024,
                }
            ),
            ModuleQueryBatchKey::from_batch(
                &selectors,
                &LeanWorkerOutputBudgets {
                    per_field_bytes: 8 * 1024,
                    total_bytes: 64 * 1024,
                }
            )
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
                    locals_raw: false,
                }],
                &budgets
            ),
            ModuleQueryBatchKey::from_batch(
                &[LeanWorkerModuleQuerySelector::ProofStateInDeclaration {
                    id: "proof_state".into(),
                    declaration: "A.two".into(),
                    position: LeanWorkerProofPositionSelector::default(),
                    locals_raw: false,
                }],
                &budgets
            )
        );
    }
}
