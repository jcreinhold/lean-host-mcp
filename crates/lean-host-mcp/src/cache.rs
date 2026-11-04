//! Bounded module-query cache keyed on source, query, and elaboration inputs.
//!
//! Position tools never cache whole-file info trees. Entries are already
//! bounded worker outcomes. Batched proof-agent queries also keep an exact
//! host-side cache because the MCP host opens short-lived worker sessions for
//! each request, while the Lean-side snapshot cache is scoped to one session.
//!
//! There is no TTL: source bytes, elaboration options, import artifacts and
//! worker generation must match instead. Import metadata is checked before a
//! lookup and again before insertion, so a rebuild during processing cannot
//! poison subsequent hits. As with the worker's session invalidation, this
//! relies on Lake propagating dependency changes to the named imports; it does
//! not scan their entire transitive closure on each query.
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

/// Exact metadata of the resolved named imports. Unlike a maximum mtime, this
/// notices deletion, backdating, and replacement of any individual artifact.
/// It intentionally does not hash library contents on a cache lookup.
#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) struct ImportArtifactStamp(Vec<(String, Option<ArtifactFileStamp>)>);

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct ArtifactFileStamp {
    path: PathBuf,
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl ImportArtifactStamp {
    /// An unreadable artifact is uncacheable; a missing artifact is recorded
    /// explicitly so creating it changes the stamp. Roots are searched in the
    /// same order as the actor's artifact freshness check.
    pub(crate) fn capture(roots: &[PathBuf], imports: &[String]) -> Option<Self> {
        let mut stamps = Vec::with_capacity(imports.len());
        for import in imports {
            let relative = import.split('.').collect::<PathBuf>().with_extension("olean");
            let mut resolved = None;
            for root in roots {
                let path = root.join(&relative);
                let metadata = match std::fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return None,
                };
                if !metadata.is_file() {
                    return None;
                }
                #[cfg(unix)]
                let identity = {
                    use std::os::unix::fs::MetadataExt as _;
                    (metadata.dev(), metadata.ino(), metadata.ctime(), metadata.ctime_nsec())
                };
                resolved = Some(ArtifactFileStamp {
                    path,
                    length: metadata.len(),
                    modified: metadata.modified().ok()?,
                    #[cfg(unix)]
                    identity,
                });
                break;
            }
            stamps.push((import.clone(), resolved));
        }
        Some(Self(stamps))
    }
}

/// All non-source inputs to a module query. Runtime configuration is immutable
/// for a project handle; generation additionally separates recycled workers.
#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub(crate) struct ModuleQueryContext {
    request_hash: [u8; 32],
    imports: Vec<String>,
    artifacts: ImportArtifactStamp,
    worker_generation: u64,
}

impl ModuleQueryContext {
    pub(crate) fn capture(
        root: &Path,
        session_imports: &[String],
        freshness_imports: &[String],
        options: &lean_rs_worker_parent::LeanWorkerElabOptions,
        worker_generation: u64,
    ) -> Option<Self> {
        let request_hash = hash_bytes(&serde_json::to_vec(&(session_imports, freshness_imports, options)).ok()?);
        let mut imports = session_imports.to_vec();
        imports.extend_from_slice(freshness_imports);
        imports.sort();
        imports.dedup();
        let artifacts = ImportArtifactStamp::capture(&crate::lake_meta::artifact_roots(root), &imports)?;
        Some(Self {
            request_hash,
            imports,
            artifacts,
            worker_generation,
        })
    }

    /// Resample only mutable inputs after processing. A changed generation is
    /// safe to store under the resulting generation if artifacts stayed equal.
    pub(crate) fn after_processing(&self, root: &Path, worker_generation: u64) -> Option<Self> {
        let artifacts = ImportArtifactStamp::capture(&crate::lake_meta::artifact_roots(root), &self.imports)?;
        if artifacts != self.artifacts {
            return None;
        }
        Some(Self {
            worker_generation,
            ..self.clone()
        })
    }
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
        context: &ModuleQueryContext,
        query: &ModuleQueryKey,
    ) -> Option<LeanWorkerModuleQueryOutcome> {
        let key = CacheKey {
            file_path: path.to_path_buf(),
            content_hash,
            context: context.clone(),
            query: query.clone(),
        };
        self.single.lock().get(&key).cloned()
    }

    pub(crate) fn insert(
        &self,
        path: PathBuf,
        content_hash: [u8; 32],
        context: ModuleQueryContext,
        query: ModuleQueryKey,
        value: LeanWorkerModuleQueryOutcome,
    ) {
        let key = CacheKey {
            file_path: path,
            content_hash,
            context,
            query,
        };
        self.single.lock().put(key, value);
    }

    pub(crate) fn get_batch(
        &self,
        path: &Path,
        content_hash: [u8; 32],
        context: &ModuleQueryContext,
        query: &ModuleQueryBatchKey,
    ) -> Option<LeanWorkerModuleQueryBatchOutcome> {
        let key = BatchCacheKey {
            file_path: path.to_path_buf(),
            content_hash,
            context: context.clone(),
            query: query.clone(),
        };
        self.batch.lock().get(&key).cloned().map(mark_batch_cache_hit)
    }

    pub(crate) fn insert_batch(
        &self,
        path: PathBuf,
        content_hash: [u8; 32],
        context: ModuleQueryContext,
        query: ModuleQueryBatchKey,
        value: LeanWorkerModuleQueryBatchOutcome,
    ) {
        let key = BatchCacheKey {
            file_path: path,
            content_hash,
            context,
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
    use super::{ImportArtifactStamp, ModuleQueryBatchKey, ModuleQueryContext, ModuleQueryKey};
    use lean_rs_worker_parent::{
        LeanWorkerModuleQuery, LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets, LeanWorkerProofPositionSelector,
    };

    fn context(root: &std::path::Path) -> ModuleQueryContext {
        ModuleQueryContext::capture(
            root,
            &["Demo.A".into()],
            &[],
            &lean_rs_worker_parent::LeanWorkerElabOptions::default(),
            1,
        )
        .unwrap()
    }

    #[test]
    fn imported_artifact_changes_invalidate_unchanged_source() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join(".lake/build/lib/lean/Demo/A.olean");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"original").unwrap();
        let cache = super::ModuleQueryCache::with_capacity(std::num::NonZeroUsize::new(4).unwrap());
        let path = temp.path().join("Query.lean");
        let source = super::hash_bytes(b"import Demo.A");
        let before = context(temp.path());
        cache.insert(
            path.clone(),
            source,
            before.clone(),
            ModuleQueryKey::Diagnostics,
            lean_rs_worker_parent::LeanWorkerModuleQueryOutcome::Unsupported,
        );
        assert!(
            cache
                .get(&path, source, &before, &ModuleQueryKey::Diagnostics)
                .is_some()
        );

        // Deliberately backdate it: a newer-maximum-mtime check misses this.
        std::fs::write(&artifact, b"changed artifact").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let after = context(temp.path());
        assert_ne!(before, after);
        assert!(cache.get(&path, source, &after, &ModuleQueryKey::Diagnostics).is_none());
        assert!(
            before.after_processing(temp.path(), 1).is_none(),
            "rebuild during execution must not be cached"
        );
        std::fs::remove_file(&artifact).unwrap();
        let missing = context(temp.path());
        assert_ne!(after, missing);
        std::fs::write(&artifact, b"rebuilt").unwrap();
        assert_ne!(
            missing,
            context(temp.path()),
            "creating a formerly missing import invalidates"
        );
    }

    #[test]
    fn artifact_stamps_track_each_import_and_preserve_warm_unrelated_queries() {
        let temp = tempfile::tempdir().unwrap();
        let roots = [temp.path().to_path_buf()];
        let imports = ["A".into(), "B".into()];
        let a = temp.path().join("A.olean");
        let b = temp.path().join("B.olean");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let before = ImportArtifactStamp::capture(&roots, &imports).unwrap();
        std::fs::write(temp.path().join("Unrelated.olean"), b"unrelated").unwrap();
        assert_eq!(Some(before.clone()), ImportArtifactStamp::capture(&roots, &imports));
        std::fs::remove_file(a).unwrap();
        assert_ne!(Some(before), ImportArtifactStamp::capture(&roots, &imports));
    }

    #[test]
    fn query_context_includes_options_imports_and_worker_generation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let before = context(root);
        let options = lean_rs_worker_parent::LeanWorkerElabOptions::default().heartbeat_limit(1);
        assert_ne!(
            Some(before.clone()),
            ModuleQueryContext::capture(root, &["Demo.A".into()], &[], &options, 1)
        );
        assert_ne!(
            Some(before.clone()),
            ModuleQueryContext::capture(
                root,
                &["Demo.B".into()],
                &[],
                &lean_rs_worker_parent::LeanWorkerElabOptions::default(),
                1
            )
        );
        assert_ne!(
            Some(before.clone()),
            ModuleQueryContext::capture(
                root,
                &["Demo.A".into()],
                &[],
                &lean_rs_worker_parent::LeanWorkerElabOptions::default(),
                2
            )
        );
        assert_eq!(before.after_processing(root, 1), Some(before.clone()));
        assert_eq!(before.after_processing(root, 2).unwrap().worker_generation, 2);
    }

    #[test]
    fn batch_cache_also_separates_dependency_context() {
        let temp = tempfile::tempdir().unwrap();
        let cache = super::ModuleQueryCache::with_capacity(std::num::NonZeroUsize::new(4).unwrap());
        let before = context(temp.path());
        let path = temp.path().join("Query.lean");
        let hash = super::hash_bytes(b"source");
        let query = ModuleQueryBatchKey::from_batch(&[], &LeanWorkerOutputBudgets::default()).unwrap();
        cache.insert_batch(
            path.clone(),
            hash,
            before.clone(),
            query.clone(),
            lean_rs_worker_parent::LeanWorkerModuleQueryBatchOutcome::Unsupported,
        );
        assert!(cache.get_batch(&path, hash, &before, &query).is_some());
        let after = before.after_processing(temp.path(), 2).unwrap();
        assert!(cache.get_batch(&path, hash, &after, &query).is_none());
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
        let cache = super::ModuleQueryCache::with_capacity(std::num::NonZeroUsize::new(4).unwrap());
        let path = std::path::Path::new("/demo/Basic.lean");
        let hash = super::hash_bytes(b"theorem t : True := trivial");
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path());
        let stored = ModuleQueryKey::from_query(&LeanWorkerModuleQuery::TypeAt { line: 1, column: 2 }).unwrap();

        cache.insert(
            path.to_path_buf(),
            hash,
            context.clone(),
            stored.clone(),
            lean_rs_worker_parent::LeanWorkerModuleQueryOutcome::Unsupported,
        );

        assert!(
            cache.get(path, hash, &context, &stored).is_some(),
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
                cache.get(path, hash, &context, &key).is_none(),
                "{other:?} read a foreign entry"
            );
        }
        // Same question, edited file: the content hash is the invalidation, so
        // this must miss without any TTL involved.
        assert!(
            cache
                .get(path, super::hash_bytes(b"edited"), &context, &stored)
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
