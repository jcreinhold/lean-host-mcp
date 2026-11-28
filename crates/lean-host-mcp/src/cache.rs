//! Bounded module-query cache keyed on source, query, and elaboration inputs.
//!
//! Position tools never cache whole-file info trees. Entries are already
//! bounded worker outcomes. Batched proof-agent queries also keep an exact
//! host-side cache because the MCP host opens short-lived worker sessions for
//! each request, while the Lean-side snapshot cache is scoped to one session.
//!
//! There is no TTL. A key holds the source bytes, the elaboration inputs, and
//! the finished-build count of the request's read turn. Each entry also names
//! the worker generation that computed it and answers only while that
//! generation lives: the worker holds the shared kernel lock for its whole
//! life, so no writer has changed the artifacts it read. A probe made while
//! another generation lives, or none, drops the entry. The count alone is not
//! enough, since a short build in another process can end between two reaper
//! ticks and never move it.
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

use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerModuleCacheStatus, LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchOutcome,
    LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector, LeanWorkerModuleQueryTimings, LeanWorkerOutputBudgets,
};
use lru::LruCache;
use parking_lot::Mutex;

/// All non-source inputs to a module query: the imports and options it asks
/// with, and the finished-build count of the request's read turn.
#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) struct ModuleQueryContext {
    request_hash: [u8; 32],
    builds_finished: u64,
}

impl ModuleQueryContext {
    pub(crate) fn capture(
        session_imports: &[String],
        freshness_imports: &[String],
        options: &lean_rs_worker_parent::LeanWorkerElabOptions,
        builds_finished: u64,
    ) -> Option<Self> {
        let request_hash = hash_bytes(&serde_json::to_vec(&(session_imports, freshness_imports, options)).ok()?);
        Some(Self {
            request_hash,
            builds_finished,
        })
    }
}

/// The worker generation to file a call's result under: the one the call
/// ended on. `None` when the worker did not complete the call reliably. A
/// crash can return an empty outcome alongside restart facts; caching that
/// value would strip the failure on the next hit, which uses fresh runtime
/// facts.
pub(crate) fn completed_generation(runtime: &crate::envelope::RuntimeFacts) -> Option<u64> {
    crate::diagnosis::execution_taint(runtime)
        .is_none()
        .then_some(runtime.worker_generation)
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct CacheKey {
    file_path: PathBuf,
    content_hash: [u8; 32],
    context: ModuleQueryContext,
    query: ModuleQueryKey,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct BatchCacheKey {
    file_path: PathBuf,
    content_hash: [u8; 32],
    context: ModuleQueryContext,
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

/// A cached answer and the worker generation that computed it.
#[derive(Debug)]
struct Entry<T> {
    value: T,
    worker_generation: u64,
}

/// The cached answer for `key` when the generation that computed it is
/// `live_generation`. Drops any other entry: its generation has retired.
fn probe<K: Hash + Eq, T: Clone>(
    cache: &Mutex<LruCache<K, Entry<T>>>,
    key: &K,
    live_generation: Option<u64>,
) -> Option<T> {
    let mut cache = cache.lock();
    let entry = cache.get(key)?;
    if Some(entry.worker_generation) == live_generation {
        return Some(entry.value.clone());
    }
    cache.pop(key);
    None
}

#[derive(Debug)]
pub(crate) struct ModuleQueryCache {
    single: Mutex<LruCache<CacheKey, Entry<LeanWorkerModuleQueryOutcome>>>,
    batch: Mutex<LruCache<BatchCacheKey, Entry<LeanWorkerModuleQueryBatchOutcome>>>,
}

impl ModuleQueryCache {
    pub(crate) fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            single: Mutex::new(LruCache::new(cap)),
            batch: Mutex::new(LruCache::new(cap)),
        }
    }

    /// `live_generation` is the generation of the project's live worker, or
    /// `None` when no worker lives.
    pub(crate) fn get(
        &self,
        path: &Path,
        content_hash: [u8; 32],
        context: &ModuleQueryContext,
        query: &ModuleQueryKey,
        live_generation: Option<u64>,
    ) -> Option<LeanWorkerModuleQueryOutcome> {
        let key = CacheKey {
            file_path: path.to_path_buf(),
            content_hash,
            context: context.clone(),
            query: query.clone(),
        };
        probe(&self.single, &key, live_generation)
    }

    pub(crate) fn insert(
        &self,
        path: PathBuf,
        content_hash: [u8; 32],
        context: ModuleQueryContext,
        query: ModuleQueryKey,
        worker_generation: u64,
        value: LeanWorkerModuleQueryOutcome,
    ) {
        let key = CacheKey {
            file_path: path,
            content_hash,
            context,
            query,
        };
        self.single.lock().put(
            key,
            Entry {
                value,
                worker_generation,
            },
        );
    }

    /// As [`Self::get`], for a batch.
    pub(crate) fn get_batch(
        &self,
        path: &Path,
        content_hash: [u8; 32],
        context: &ModuleQueryContext,
        query: &ModuleQueryBatchKey,
        live_generation: Option<u64>,
    ) -> Option<LeanWorkerModuleQueryBatchOutcome> {
        let key = BatchCacheKey {
            file_path: path.to_path_buf(),
            content_hash,
            context: context.clone(),
            query: query.clone(),
        };
        probe(&self.batch, &key, live_generation).map(mark_batch_cache_hit)
    }

    pub(crate) fn insert_batch(
        &self,
        path: PathBuf,
        content_hash: [u8; 32],
        context: ModuleQueryContext,
        query: ModuleQueryBatchKey,
        worker_generation: u64,
        value: LeanWorkerModuleQueryBatchOutcome,
    ) {
        let key = BatchCacheKey {
            file_path: path,
            content_hash,
            context,
            query,
        };
        self.batch.lock().put(
            key,
            Entry {
                value,
                worker_generation,
            },
        );
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
    use std::path::Path;

    use super::{ModuleQueryBatchKey, ModuleQueryCache, ModuleQueryContext, ModuleQueryKey, completed_generation};
    use lean_rs_worker_parent::{
        LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryOutcome,
        LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets, LeanWorkerProofPositionSelector,
    };

    fn context(builds_finished: u64) -> ModuleQueryContext {
        ModuleQueryContext::capture(
            &["Demo.A".into()],
            &[],
            &lean_rs_worker_parent::LeanWorkerElabOptions::default(),
            builds_finished,
        )
        .unwrap()
    }

    fn cache() -> ModuleQueryCache {
        ModuleQueryCache::with_capacity(std::num::NonZeroUsize::new(4).unwrap())
    }

    fn batch_key() -> ModuleQueryBatchKey {
        ModuleQueryBatchKey::from_batch(&[], &LeanWorkerOutputBudgets::default()).unwrap()
    }

    /// A write turn that finished after the entry was stored changes the key,
    /// so even the live generation's answer misses.
    #[test]
    fn a_finished_build_misses_unchanged_source() {
        let cache = cache();
        let path = Path::new("/demo/Query.lean");
        let source = super::hash_bytes(b"import Demo.A");
        let before = context(0);
        cache.insert(
            path.to_path_buf(),
            source,
            before.clone(),
            ModuleQueryKey::Diagnostics,
            1,
            LeanWorkerModuleQueryOutcome::Unsupported,
        );
        cache.insert_batch(
            path.to_path_buf(),
            source,
            before.clone(),
            batch_key(),
            1,
            LeanWorkerModuleQueryBatchOutcome::Unsupported,
        );
        assert!(
            cache
                .get(path, source, &before, &ModuleQueryKey::Diagnostics, Some(1))
                .is_some()
        );
        assert!(cache.get_batch(path, source, &before, &batch_key(), Some(1)).is_some());
        let after = context(1);
        assert!(
            cache
                .get(path, source, &after, &ModuleQueryKey::Diagnostics, Some(1))
                .is_none()
        );
        assert!(cache.get_batch(path, source, &after, &batch_key(), Some(1)).is_none());
    }

    /// An entry answers only while the generation that computed it lives:
    /// that worker's shared lock has kept every writer out since it loaded.
    /// After it retires, a build in another process may have ended between
    /// two reaper ticks without moving the count, so the entry goes.
    #[test]
    fn a_retired_generations_cache_entries_miss() {
        let cache = cache();
        let path = Path::new("/demo/Query.lean");
        let source = super::hash_bytes(b"import Demo.A");
        let context = context(0);
        let key = ModuleQueryKey::Diagnostics;
        for live in [None, Some(2)] {
            cache.insert(
                path.to_path_buf(),
                source,
                context.clone(),
                key.clone(),
                1,
                LeanWorkerModuleQueryOutcome::Unsupported,
            );
            cache.insert_batch(
                path.to_path_buf(),
                source,
                context.clone(),
                batch_key(),
                1,
                LeanWorkerModuleQueryBatchOutcome::Unsupported,
            );
            assert!(cache.get(path, source, &context, &key, Some(1)).is_some());
            assert!(cache.get_batch(path, source, &context, &batch_key(), Some(1)).is_some());
            assert!(cache.get(path, source, &context, &key, live).is_none(), "{live:?}");
            assert!(
                cache.get_batch(path, source, &context, &batch_key(), live).is_none(),
                "{live:?}"
            );
            // The miss dropped both entries, so they do not come back.
            assert!(cache.get(path, source, &context, &key, Some(1)).is_none(), "{live:?}");
            assert!(
                cache.get_batch(path, source, &context, &batch_key(), Some(1)).is_none(),
                "{live:?}"
            );
        }
    }

    #[test]
    fn query_context_includes_options_imports_and_build_count() {
        let before = context(0);
        let project_options = lean_rs_worker_parent::LeanWorkerElabOptions::default().lean_options(
            std::collections::BTreeMap::from([("autoImplicit".into(), lean_toolchain::LeanOptionValue::Bool(false))]),
        );
        assert_ne!(
            Some(before.clone()),
            ModuleQueryContext::capture(&["Demo.A".into()], &[], &project_options, 0)
        );
        let options = lean_rs_worker_parent::LeanWorkerElabOptions::default().heartbeat_limit(1);
        assert_ne!(
            Some(before.clone()),
            ModuleQueryContext::capture(&["Demo.A".into()], &[], &options, 0)
        );
        assert_ne!(
            Some(before.clone()),
            ModuleQueryContext::capture(
                &["Demo.B".into()],
                &[],
                &lean_rs_worker_parent::LeanWorkerElabOptions::default(),
                0
            )
        );
        assert_ne!(before, context(1));
        assert_eq!(before, context(0));
    }

    #[test]
    fn interrupted_call_cannot_cache_empty_outcome_for_retry() {
        use crate::envelope::{RuntimeFacts, RuntimeRestartEvent};
        let cache = cache();
        let context = context(0);
        let path = Path::new("/demo/Query.lean");
        let hash = super::hash_bytes(b"source containing an error");
        for cause in [
            "child_abort",
            "child_exit",
            "timeout",
            "cancelled",
            "worker_internal",
            "session_missing",
        ] {
            let runtime = RuntimeFacts {
                worker_generation: 2,
                call_restart: Some(RuntimeRestartEvent {
                    cause: cause.into(),
                    ..RuntimeRestartEvent::default()
                }),
                ..RuntimeFacts::default()
            };
            if let Some(generation) = completed_generation(&runtime) {
                cache.insert_batch(
                    path.to_path_buf(),
                    hash,
                    context.clone(),
                    batch_key(),
                    generation,
                    LeanWorkerModuleQueryBatchOutcome::Unsupported,
                );
            }
            assert!(
                cache.get_batch(path, hash, &context, &batch_key(), Some(2)).is_none(),
                "{cause}"
            );
        }
    }

    #[test]
    fn retry_cache_requires_explicit_completed_final_attempt() {
        use crate::envelope::{FinalAttemptStatus, RuntimeFacts, RuntimeRestartEvent};
        for status in [
            FinalAttemptStatus::Unknown,
            FinalAttemptStatus::Interrupted,
            FinalAttemptStatus::Completed,
        ] {
            let mut runtime = RuntimeFacts {
                worker_generation: 2,
                retry_count: 1,
                final_attempt: status,
                previous_attempt_failure: Some(RuntimeRestartEvent {
                    cause: "child_abort".into(),
                    ..RuntimeRestartEvent::default()
                }),
                ..RuntimeFacts::default()
            };
            let completed = (status == FinalAttemptStatus::Completed).then_some(2);
            assert_eq!(completed_generation(&runtime), completed);
            // An abnormal replacement before a completed attempt leaves its
            // value whole; before an unfinished one it does not.
            runtime.previous_attempt_failure = None;
            runtime.call_restart = Some(RuntimeRestartEvent {
                cause: "child_abort".into(),
                ..RuntimeRestartEvent::default()
            });
            assert_eq!(completed_generation(&runtime), completed);
        }
    }

    #[test]
    fn completed_calls_cache_after_planned_recycle_or_historical_crash() {
        use crate::envelope::{RuntimeFacts, RuntimeRestartEvent};
        for cause in ["import_residue", "import_residue_idle", "artifacts_rebuilt"] {
            let runtime = RuntimeFacts {
                worker_generation: 2,
                call_restart: Some(RuntimeRestartEvent {
                    cause: cause.into(),
                    planned: true,
                    ..RuntimeRestartEvent::default()
                }),
                ..RuntimeFacts::default()
            };
            assert_eq!(completed_generation(&runtime), Some(2), "{cause}");
        }
        let runtime = RuntimeFacts {
            worker_generation: 2,
            last_restart: Some(RuntimeRestartEvent {
                cause: "child_abort".into(),
                ..RuntimeRestartEvent::default()
            }),
            ..RuntimeFacts::default()
        };
        assert_eq!(completed_generation(&runtime), Some(2));
    }

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
        let cache = cache();
        let path = Path::new("/demo/Basic.lean");
        let hash = super::hash_bytes(b"theorem t : True := trivial");
        let context = context(0);
        let stored = ModuleQueryKey::from_query(&LeanWorkerModuleQuery::TypeAt { line: 1, column: 2 }).unwrap();

        cache.insert(
            path.to_path_buf(),
            hash,
            context.clone(),
            stored.clone(),
            1,
            LeanWorkerModuleQueryOutcome::Unsupported,
        );

        assert!(
            cache.get(path, hash, &context, &stored, Some(1)).is_some(),
            "same question, same answer"
        );
        for other in [
            LeanWorkerModuleQuery::Diagnostics,
            LeanWorkerModuleQuery::GoalAt { line: 1, column: 2 },
            LeanWorkerModuleQuery::TypeAt { line: 9, column: 9 },
            LeanWorkerModuleQuery::References { name: "Nat.add".into() },
        ] {
            let key = ModuleQueryKey::from_query(&other).unwrap();
            assert!(
                cache.get(path, hash, &context, &key, Some(1)).is_none(),
                "{other:?} read a foreign entry"
            );
        }
        // Same question, edited file: the content hash is the invalidation, so
        // this must miss without any TTL involved.
        assert!(
            cache
                .get(path, super::hash_bytes(b"edited"), &context, &stored, Some(1))
                .is_none()
        );
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
