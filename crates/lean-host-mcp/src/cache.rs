//! In-memory cache of `LeanWorkerProcessedFile` projections keyed on path +
//! content hash.
//!
//! Position-based tools (`goal_at_position`, `type_at_position`,
//! `references_of_name`, `file_diagnostics`) want cheap repeat lookups against
//! the same source file: a cursor that moves twenty times per minute should
//! not re-elaborate the file twenty times. The cache lives on
//! [`ToolContext`](crate::tools::ToolContext) and is consulted on every call.
//!
//! Keying invariant: the SHA-256 of the file contents is part of the key, so
//! any edit (even a whitespace change) misses and re-processes. Imports are
//! deliberately *not* in the key because position tools derive them from the
//! file header; changing the header changes the file bytes and misses
//! structurally.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lean_rs_worker_parent::{LeanWorkerNameRef, LeanWorkerProcessedFile, LeanWorkerTacticInfo, LeanWorkerTermInfo};
use lru::LruCache;
use parking_lot::Mutex;

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
struct CacheKey {
    file_path: PathBuf,
    content_hash: [u8; 32],
}

/// Bounded LRU of [`LeanWorkerProcessedFile`] projections keyed on
/// `(file_path, sha256(contents))`. Any edit to the source bytes misses
/// structurally, so stale entries are impossible.
#[derive(Debug)]
pub struct ProcessedFileCache {
    inner: Mutex<LruCache<CacheKey, Arc<LeanWorkerProcessedFile>>>,
}

impl ProcessedFileCache {
    pub fn with_capacity(cap: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn get(&self, path: &Path, content_hash: [u8; 32]) -> Option<Arc<LeanWorkerProcessedFile>> {
        let key = CacheKey {
            file_path: path.to_path_buf(),
            content_hash,
        };
        let mut guard = self.inner.lock();
        guard.get(&key).map(Arc::clone)
    }

    pub fn insert(&self, path: PathBuf, content_hash: [u8; 32], value: Arc<LeanWorkerProcessedFile>) {
        let key = CacheKey {
            file_path: path,
            content_hash,
        };
        let mut guard = self.inner.lock();
        guard.put(key, value);
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

/// Innermost tactic node containing `(line, column)`.
///
/// Containment test: `(start, end]` on the start side and `[end, end)` on
/// the end side. Linear scan is fine for typical Lean files (well under
/// 1000 tactic nodes); see the `position_lookup_after_cache_warm` bench.
#[must_use]
pub fn tactic_at(file: &LeanWorkerProcessedFile, line: u32, column: u32) -> Option<&LeanWorkerTacticInfo> {
    let mut best: Option<&LeanWorkerTacticInfo> = None;
    for node in &file.tactics {
        if !contains(
            (node.start_line, node.start_column),
            (node.end_line, node.end_column),
            line,
            column,
        ) {
            continue;
        }
        best = Some(match best {
            None => node,
            Some(prev) if narrower(node, prev) => node,
            Some(prev) => prev,
        });
    }
    best
}

/// Innermost term node containing `(line, column)`.
#[must_use]
pub fn term_at(file: &LeanWorkerProcessedFile, line: u32, column: u32) -> Option<&LeanWorkerTermInfo> {
    let mut best: Option<&LeanWorkerTermInfo> = None;
    for node in &file.terms {
        if !contains(
            (node.start_line, node.start_column),
            (node.end_line, node.end_column),
            line,
            column,
        ) {
            continue;
        }
        best = Some(match best {
            None => node,
            Some(prev) if term_narrower(node, prev) => node,
            Some(prev) => prev,
        });
    }
    best
}

/// Every name occurrence whose qualified name matches `name`.
pub fn references_of<'a>(
    file: &'a LeanWorkerProcessedFile,
    name: &'a str,
) -> impl Iterator<Item = &'a LeanWorkerNameRef> + 'a {
    file.names.iter().filter(move |n| n.name == name)
}

fn contains(start: (u32, u32), end: (u32, u32), line: u32, col: u32) -> bool {
    let after_start = (line, col) >= start;
    let before_end = (line, col) <= end;
    after_start && before_end
}

fn narrower(a: &LeanWorkerTacticInfo, b: &LeanWorkerTacticInfo) -> bool {
    span_size_tactic(a) < span_size_tactic(b)
}

fn term_narrower(a: &LeanWorkerTermInfo, b: &LeanWorkerTermInfo) -> bool {
    span_size_term(a) < span_size_term(b)
}

fn span_size_tactic(n: &LeanWorkerTacticInfo) -> (u32, u32) {
    span_size(n.start_line, n.start_column, n.end_line, n.end_column)
}

fn span_size_term(n: &LeanWorkerTermInfo) -> (u32, u32) {
    span_size(n.start_line, n.start_column, n.end_line, n.end_column)
}

fn span_size(sl: u32, sc: u32, el: u32, ec: u32) -> (u32, u32) {
    (el.saturating_sub(sl), if el == sl { ec.saturating_sub(sc) } else { ec })
}
