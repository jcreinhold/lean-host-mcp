//! Resolve a Lake project's `lean-toolchain` pin to the matching
//! per-toolchain worker binary on disk.
//!
//! Two value types and one error live here:
//!
//! - [`ToolchainId`] is the canonical short form of a toolchain pin
//!   (e.g. `v4.30.0-rc2`, `nightly-2026-05-20`). [`Self::parse`] accepts
//!   either the bare short form or the elan-style `leanprover/lean4:<id>`.
//!   [`Self::from_lake_root`] reads `<root>/lean-toolchain` and parses it.
//! - [`WorkerBinary`] is the resolved path to a worker binary that links
//!   the corresponding Lean shared library.
//!   [`Self::resolve_for`] consults (in order) the
//!   `LEAN_HOST_MCP_WORKERS_DIR` developer override, then
//!   `<install_root>/<id>/lean-host-mcp-worker`. Missing → an actionable
//!   [`ToolchainError::WorkerNotInstalled`] whose `install_cmd` field
//!   names the exact `install-worker` invocation that will produce it.
//! - [`ToolchainError`] is the typed failure surface. Project-open code
//!   maps it into [`crate::error::ServerError::BadProject`] so the install
//!   command flows through to the client.

use std::fmt;
use std::path::{Path, PathBuf};

/// Canonical short form of a Lean toolchain pin (e.g. `v4.30.0-rc2`,
/// `nightly-2026-05-20`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolchainId(String);

/// File name of the per-toolchain worker binary inside
/// [`WorkerBinary::install_root`].
pub const WORKER_FILE_NAME: &str = "lean-host-mcp-worker";

/// Env var that lets a developer running `cargo run` point the parent at
/// a worker binary outside the standard install layout.
pub const WORKERS_DIR_ENV: &str = "LEAN_HOST_MCP_WORKERS_DIR";

impl ToolchainId {
    /// Parse from a `lean-toolchain` line. Accepts the elan-style
    /// `leanprover/lean4:<id>` and the bare `<id>` short form.
    ///
    /// # Errors
    ///
    /// [`ToolchainError::UnparseableToolchainString`] if the input is
    /// empty, contains whitespace or other unexpected characters, or
    /// names a Lean fork we do not understand.
    pub fn parse(raw: &str) -> Result<Self, ToolchainError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ToolchainError::UnparseableToolchainString(raw.to_owned()));
        }
        let short = if let Some(rest) = trimmed.strip_prefix("leanprover/lean4:") {
            rest
        } else if trimmed.contains(':') || trimmed.contains('/') {
            return Err(ToolchainError::UnparseableToolchainString(raw.to_owned()));
        } else {
            trimmed
        };
        if short.is_empty() || short.chars().any(char::is_whitespace) {
            return Err(ToolchainError::UnparseableToolchainString(raw.to_owned()));
        }
        Ok(Self(short.to_owned()))
    }

    /// Read `<root>/lean-toolchain` and parse it.
    ///
    /// # Errors
    ///
    /// [`ToolchainError::LeanToolchainFileMissing`] if the file is absent
    /// or unreadable. Forwards [`Self::parse`]'s error otherwise.
    pub fn from_lake_root(root: &Path) -> Result<Self, ToolchainError> {
        let path = root.join("lean-toolchain");
        let contents = std::fs::read_to_string(&path)
            .map_err(|_| ToolchainError::LeanToolchainFileMissing(path.clone()))?;
        Self::parse(&contents)
    }

    /// Resolved path to the elan toolchain root
    /// (`~/.elan/toolchains/leanprover--lean4---<id>`).
    ///
    /// # Errors
    ///
    /// [`ToolchainError::ElanToolchainNotInstalled`] if the directory is
    /// absent. Returns the path even when present so callers can build
    /// further on top of it.
    pub fn elan_dir(&self) -> Result<PathBuf, ToolchainError> {
        let home = dirs::home_dir().ok_or_else(|| ToolchainError::ElanToolchainNotInstalled {
            toolchain: self.clone(),
            elan_dir: PathBuf::from(format!("~/.elan/toolchains/leanprover--lean4---{}", self.0)),
        })?;
        let dir = home
            .join(".elan")
            .join("toolchains")
            .join(format!("leanprover--lean4---{}", self.0));
        if dir.is_dir() {
            Ok(dir)
        } else {
            Err(ToolchainError::ElanToolchainNotInstalled {
                toolchain: self.clone(),
                elan_dir: dir,
            })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolchainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolved path to a per-toolchain worker binary.
#[derive(Debug, Clone)]
pub struct WorkerBinary {
    pub path: PathBuf,
    pub toolchain: ToolchainId,
}

impl WorkerBinary {
    /// Look up the worker binary for `toolchain`.
    ///
    /// Resolution order:
    ///
    /// 1. If `LEAN_HOST_MCP_WORKERS_DIR` is set and
    ///    `<dir>/lean-host-mcp-worker` exists, return that.
    /// 2. If `LEAN_HOST_MCP_WORKERS_DIR` is set and
    ///    `<dir>/<id>/lean-host-mcp-worker` exists, return that.
    /// 3. Otherwise look under [`Self::install_root`].
    ///
    /// # Errors
    ///
    /// [`ToolchainError::WorkerNotInstalled`] when no candidate exists.
    /// The error carries the exact `install-worker` command needed to fix
    /// the situation.
    pub fn resolve_for(toolchain: &ToolchainId) -> Result<Self, ToolchainError> {
        let override_dir = std::env::var_os(WORKERS_DIR_ENV).map(PathBuf::from);
        Self::resolve_with_override(toolchain, override_dir.as_deref())
    }

    /// Resolution variant that lets the caller inject the
    /// `LEAN_HOST_MCP_WORKERS_DIR` value rather than reading the env.
    /// Used by the test suite (env mutation is forbidden by the
    /// workspace `unsafe-code` lint) and by tooling that wants to
    /// override resolution without touching the process environment.
    ///
    /// # Errors
    ///
    /// Same as [`Self::resolve_for`].
    pub fn resolve_with_override(
        toolchain: &ToolchainId,
        override_dir: Option<&Path>,
    ) -> Result<Self, ToolchainError> {
        if let Some(dir) = override_dir {
            let bare = dir.join(WORKER_FILE_NAME);
            if bare.is_file() {
                return Ok(Self {
                    path: bare,
                    toolchain: toolchain.clone(),
                });
            }
            let with_id = dir.join(toolchain.as_str()).join(WORKER_FILE_NAME);
            if with_id.is_file() {
                return Ok(Self {
                    path: with_id,
                    toolchain: toolchain.clone(),
                });
            }
            return Err(Self::not_installed(toolchain));
        }
        let candidate = Self::install_root().join(toolchain.as_str()).join(WORKER_FILE_NAME);
        if candidate.is_file() {
            Ok(Self {
                path: candidate,
                toolchain: toolchain.clone(),
            })
        } else {
            Err(Self::not_installed(toolchain))
        }
    }

    /// `~/.local/share/lean-host-mcp/workers` (or
    /// `$XDG_DATA_HOME/lean-host-mcp/workers`).
    ///
    /// Falls back to the current directory if no data dir can be located —
    /// in that situation the calling code will fail soon after with a
    /// concrete `WorkerNotInstalled`.
    #[must_use]
    pub fn install_root() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("lean-host-mcp")
            .join("workers")
    }

    fn not_installed(toolchain: &ToolchainId) -> ToolchainError {
        ToolchainError::WorkerNotInstalled {
            toolchain: toolchain.clone(),
            install_cmd: format!("lean-host-mcp install-worker --toolchain {}", toolchain.as_str()),
        }
    }
}

/// Typed failures during toolchain resolution. Project-open code maps
/// these into [`crate::error::ServerError::BadProject`].
#[derive(Debug)]
pub enum ToolchainError {
    UnparseableToolchainString(String),
    LeanToolchainFileMissing(PathBuf),
    ElanToolchainNotInstalled {
        toolchain: ToolchainId,
        elan_dir: PathBuf,
    },
    WorkerNotInstalled {
        toolchain: ToolchainId,
        install_cmd: String,
    },
}

impl fmt::Display for ToolchainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableToolchainString(raw) => {
                write!(f, "could not parse lean-toolchain string: {raw:?}")
            }
            Self::LeanToolchainFileMissing(path) => {
                write!(f, "lean-toolchain file not found at {}", path.display())
            }
            Self::ElanToolchainNotInstalled { toolchain, elan_dir } => write!(
                f,
                "elan toolchain {} is not installed (expected {})",
                toolchain,
                elan_dir.display()
            ),
            Self::WorkerNotInstalled {
                toolchain,
                install_cmd,
            } => write!(
                f,
                "no worker binary for toolchain {toolchain}; run: {install_cmd}",
            ),
        }
    }
}

impl std::error::Error for ToolchainError {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code uses unwrap/expect/panic to surface failure paths concisely"
)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn parse_accepts_elan_prefix_and_bare_short_form() {
        assert_eq!(
            ToolchainId::parse("leanprover/lean4:v4.30.0-rc2").unwrap().as_str(),
            "v4.30.0-rc2",
        );
        assert_eq!(
            ToolchainId::parse("v4.30.0-rc2").unwrap().as_str(),
            "v4.30.0-rc2",
        );
        assert_eq!(
            ToolchainId::parse("nightly-2026-05-20").unwrap().as_str(),
            "nightly-2026-05-20",
        );
        assert_eq!(
            ToolchainId::parse("  leanprover/lean4:v4.30.0-rc2  \n").unwrap().as_str(),
            "v4.30.0-rc2",
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(
            ToolchainId::parse(""),
            Err(ToolchainError::UnparseableToolchainString(_))
        ));
        assert!(matches!(
            ToolchainId::parse("v4 .30"),
            Err(ToolchainError::UnparseableToolchainString(_))
        ));
        // Unknown fork.
        assert!(matches!(
            ToolchainId::parse("acme/lean5:v6.0"),
            Err(ToolchainError::UnparseableToolchainString(_))
        ));
    }

    #[test]
    fn from_lake_root_reads_lean_toolchain_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lean-toolchain"), "leanprover/lean4:v4.30.0-rc2\n").unwrap();
        let id = ToolchainId::from_lake_root(tmp.path()).unwrap();
        assert_eq!(id.as_str(), "v4.30.0-rc2");
    }

    #[test]
    fn from_lake_root_reports_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            ToolchainId::from_lake_root(tmp.path()),
            Err(ToolchainError::LeanToolchainFileMissing(_))
        ));
    }

    #[test]
    fn worker_binary_missing_under_override_returns_install_cmd() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0-rc2").unwrap();
        let err = WorkerBinary::resolve_with_override(&id, Some(tmp.path())).unwrap_err();
        match err {
            ToolchainError::WorkerNotInstalled { install_cmd, .. } => {
                assert!(install_cmd.contains("v4.30.0-rc2"), "got: {install_cmd}");
            }
            ToolchainError::UnparseableToolchainString(_)
            | ToolchainError::LeanToolchainFileMissing(_)
            | ToolchainError::ElanToolchainNotInstalled { .. } => {
                panic!("unexpected ToolchainError variant");
            }
        }
    }

    #[test]
    fn worker_binary_with_id_subdir_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0-rc2").unwrap();
        let nested = tmp.path().join("v4.30.0-rc2");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let resolved = WorkerBinary::resolve_with_override(&id, Some(tmp.path())).unwrap();
        assert_eq!(resolved.path, nested.join(WORKER_FILE_NAME));
    }

    #[test]
    fn worker_binary_bare_developer_fallback_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0-rc2").unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let resolved = WorkerBinary::resolve_with_override(&id, Some(tmp.path())).unwrap();
        assert_eq!(resolved.path, tmp.path().join(WORKER_FILE_NAME));
    }
}
