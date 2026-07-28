//! Shared Lean source-file ingestion for proof-agent tools.
//!
//! **One [`read_query_file`] per handler.** A [`QueryFile`] is the currency
//! every downstream helper takes; nothing below a handler re-derives one from a
//! path. Two reasons, in order of weight:
//!
//! 1. *Correctness.* The bytes, the content hash, and the header-import scan
//!    must describe one snapshot. A second read can see an edit the first did
//!    not, which would put a cache key, an elaboration, and a reported import
//!    list out of step with each other — a disagreement no caller could detect.
//! 2. *Cost.* Measured on this machine (release): **60 µs** for a 3.6 KB file
//!    and **381 µs** for a 191 KB one — roughly 57 µs of fixed path
//!    canonicalization and syscalls plus ~1.7 µs/KiB of read, hash, and header
//!    scan. That is invisible next to an elaboration, but a second read costs
//!    *more than the entire* warm module-query cache hit it would precede,
//!    which the repo targets at under 50 µs. Handing the struct down instead
//!    trades those syscalls for a memcpy of the source.
//!
//! `lean_verify`'s `file_all` group is the one place a file is still read twice,
//! because the two reads are in different *handlers*: it invokes
//! `declaration_inventory` as a tool and then prepares its own verify group.
//! Removing it would mean threading a `QueryFile` out through a public tool
//! response, and the duplicate is followed by elaborating every declaration the
//! inventory found — sub-millisecond against a path that is tens of
//! milliseconds at best. Not worth the surface.

use std::path::{Path, PathBuf};

use crate::error::{Result, ServerError};

/// One snapshot of a Lean source file: the bytes, their content hash, and the
/// header-import scan of those same bytes. Produced only by
/// [`read_query_file`], so the three can never describe different reads.
pub(crate) struct QueryFile {
    pub resolved: PathBuf,
    pub hash: [u8; 32],
    pub imports: Vec<String>,
    pub source: String,
}

pub(crate) fn read_query_file(root: &Path, path: &Path) -> Result<QueryFile> {
    let resolved = resolve_path(root, path).canonicalize().map_err(ServerError::Io)?;
    let bytes = std::fs::read(&resolved).map_err(ServerError::Io)?;
    let hash = crate::cache::hash_bytes(&bytes);
    let source = String::from_utf8(bytes).map_err(|e| ServerError::Internal(format!("file not UTF-8: {e}")))?;
    let imports = header_imports(&source);
    Ok(QueryFile {
        resolved,
        hash,
        imports,
        source,
    })
}

pub(crate) fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub(crate) fn module_name_for_file(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.extension()? != "lean" {
        return None;
    }
    let stemmed = relative.with_extension("");
    let parts = stemmed
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    if parts.is_empty() { None } else { Some(parts.join(".")) }
}

pub(crate) fn source_path_for_module(root: &Path, module: &str) -> PathBuf {
    let relative: PathBuf = module.split('.').collect();
    root.join(relative).with_extension("lean")
}

pub(crate) fn header_imports(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.split_once("--").map_or(line, |(before, _)| before);
            let mut words = line.split_whitespace();
            let mut token = words.next()?;
            if token == "public" {
                token = words.next()?;
            }
            if token == "meta" {
                token = words.next()?;
            }
            if token != "import" {
                return None;
            }
            if words.clone().next() == Some("all") {
                let _ = words.next();
            }
            words.next().map(str::to_owned)
        })
        .collect()
}
