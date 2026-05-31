//! Resolve a Lake project's `lean-toolchain` pin to the matching
//! per-toolchain worker binary on disk.
//!
//! Two value types and one error live here:
//!
//! - [`ToolchainId`] is the canonical short form of a toolchain pin
//!   (e.g. `v4.30.0`, `nightly-2026-05-20`). [`ToolchainId::parse`] accepts
//!   either the bare short form or the elan-style `leanprover/lean4:<id>`.
//!   [`ToolchainId::from_lake_root`] reads `<root>/lean-toolchain` and parses it.
//! - [`WorkerBinary`] is the resolved path to a worker binary that links
//!   the corresponding Lean shared library.
//!   [`WorkerBinary::resolve_for`] consults (in order) the
//!   `LEAN_HOST_MCP_WORKERS_DIR` developer override, then
//!   `<install_root>/<id>/lean-host-mcp-worker`. Missing produces an
//!   actionable [`ToolchainError::WorkerNotInstalled`] whose `install_cmd`
//!   field names the exact `install-worker` invocation that will produce it.
//! - [`ToolchainError`] is the typed failure surface. Project-open code
//!   maps it into [`crate::error::ServerError::BadProject`] so the install
//!   command flows through to the client.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::smoke::SmokeOutcome;

/// Canonical short form of a Lean toolchain pin (e.g. `v4.30.0`,
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
        let contents =
            std::fs::read_to_string(&path).map_err(|_| ToolchainError::LeanToolchainFileMissing(path.clone()))?;
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

    /// Classify this pin against the lean-rs supported window.
    ///
    /// Pure: reads only [`lean_toolchain::SUPPORTED_TOOLCHAINS`], no IO. The
    /// single leading `v` is stripped before the query (`v4.31.0-rc1` ⇒
    /// `4.31.0-rc1`, the bare form lean-rs tabulates). A pin that does not
    /// parse as `X.Y.Z[-rcN]` (e.g. `nightly-2026-05-20`) is
    /// [`WindowVerdict::Unknown`] — allowed, never a crash.
    ///
    /// Used by `install-worker` *before* it spends minutes building (the
    /// worker does not exist yet, so the full [`WorkerBinary::resolve_ready_for`]
    /// gate cannot answer), by `--list`, and internally by the gate.
    #[must_use]
    pub fn window_verdict(&self) -> WindowVerdict {
        let bare = self.0.strip_prefix('v').unwrap_or(&self.0);
        if lean_toolchain::supported_for(bare).is_some() {
            return WindowVerdict::Supported;
        }
        match version_key(bare) {
            Some(pin) => {
                let (window, nearest) = out_of_window_bounds(pin);
                WindowVerdict::OutOfWindow { window, nearest }
            }
            None => WindowVerdict::Unknown,
        }
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
    pub fn resolve_with_override(toolchain: &ToolchainId, override_dir: Option<&Path>) -> Result<Self, ToolchainError> {
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
    /// Falls back to the current directory if no data dir can be located;
    /// the calling code will then fail soon after with a concrete
    /// `WorkerNotInstalled`.
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
            install_cmd: install_cmd(toolchain),
        }
    }

    /// The one answer project-open needs about a pinned toolchain: window
    /// membership, elan + worker install presence, and header-digest
    /// provenance, folded into a single [`Readiness`] verdict the caller maps
    /// to one outcome (spawn, warn-and-spawn, or a typed `BadProject`).
    ///
    /// This hides five independently-volatile decisions behind one call — the
    /// window source ([`ToolchainId::window_verdict`]), the elan layout
    /// ([`ToolchainId::elan_dir`]), the worker install layout
    /// ([`Self::resolve_with_override`]), the header-digest provenance
    /// mechanism, and the recorded runtime smoke result (both via the private
    /// `WorkerSidecar`) — so no call site consults a classifier and a separate
    /// provenance check. An out-of-window pin short-circuits before any
    /// filesystem probe, so the caller gets the window message even when the
    /// bogus toolchain was never installed.
    ///
    /// On the happy path it hashes `<elan_dir>/include/lean/lean.h` once (a
    /// few-KB SHA-256); this belongs on the cold resolve/open path, not the
    /// warm per-call path. The `LEAN_HOST_MCP_WORKERS_DIR` override is read
    /// from the environment.
    #[must_use]
    pub fn resolve_ready_for(pin: &ToolchainId) -> Readiness {
        // Window first: an out-of-window pin can never load, and the bogus
        // toolchain is usually not installed, so checking elan first would
        // bury the useful "outside the window" message.
        if let WindowVerdict::OutOfWindow { window, nearest } = pin.window_verdict() {
            return Readiness::Unsupported { window, nearest };
        }
        let elan_dir = match pin.elan_dir() {
            Ok(dir) => dir,
            Err(ToolchainError::ElanToolchainNotInstalled { toolchain, elan_dir }) => {
                return Readiness::ToolchainNotInstalled { toolchain, elan_dir };
            }
            Err(_) => {
                return Readiness::ToolchainNotInstalled {
                    toolchain: pin.clone(),
                    elan_dir: PathBuf::new(),
                };
            }
        };
        let current = hash_lean_header(&elan_dir).ok();
        let override_dir = std::env::var_os(WORKERS_DIR_ENV).map(PathBuf::from);
        Self::resolve_ready_with_override(pin, override_dir.as_deref(), elan_dir, current.as_deref())
    }

    /// Resolution variant that injects the `LEAN_HOST_MCP_WORKERS_DIR` value,
    /// the resolved `lean_sysroot` (the toolchain's `elan_dir`), and the
    /// already-computed current `lean.h` digest rather than reading the
    /// environment and filesystem. Mirrors [`Self::resolve_with_override`];
    /// the test suite drives the gate's window/install/provenance logic
    /// through this seam without a real toolchain on disk.
    #[must_use]
    pub fn resolve_ready_with_override(
        pin: &ToolchainId,
        override_dir: Option<&Path>,
        lean_sysroot: PathBuf,
        current_digest: Option<&str>,
    ) -> Readiness {
        // Self-contained for direct test callers: re-check the window so the
        // seam alone produces the full verdict set.
        let window = pin.window_verdict();
        if let WindowVerdict::OutOfWindow { window, nearest } = window {
            return Readiness::Unsupported { window, nearest };
        }
        let Ok(worker) = Self::resolve_with_override(pin, override_dir) else {
            return Readiness::NotInstalled {
                toolchain: pin.clone(),
                install_cmd: install_cmd(pin),
            };
        };
        // Provenance, in order of severity:
        //   1. header drift  → Stale   (rebuild advice trumps everything else)
        //   2. smoke failed   → Unusable (built + digest-matched, but cannot run)
        //   3. smoke missing  → Ready + soft note (older host: reinstall to verify)
        //   4. sidecar absent → Ready + soft note (older host: no provenance at all)
        let install_dir = worker.path.parent().unwrap_or(&worker.path);
        let note = match WorkerSidecar::load(install_dir) {
            Some(sidecar) => {
                // A recorded build-time digest that no longer matches the
                // toolchain's current lean.h means the header drifted under the
                // worker; a rebuild is the right move regardless of any smoke
                // verdict, so check it first.
                if let Some(current) = current_digest
                    && !sidecar.header_matches(current)
                {
                    return Readiness::Stale {
                        toolchain: pin.clone(),
                        install_cmd: install_cmd(pin),
                    };
                }
                match sidecar.smoke() {
                    // A header-digest match does not imply ABI compatibility (a
                    // toolchain's libleanshared can crash this worker); the
                    // recorded runtime smoke result is the sound signal.
                    Some(SmokeOutcome::Failed { detail }) => {
                        return Readiness::Unusable {
                            toolchain: pin.clone(),
                            detail: detail.to_owned(),
                            install_cmd: install_cmd(pin),
                        };
                    }
                    Some(SmokeOutcome::Passed) => None,
                    // Built by an older host that did not smoke-test: the header
                    // digest still guards drift, but the worker is unverified at
                    // runtime — nudge a reinstall to record a smoke result.
                    None => Some(format!(
                        "worker for {pin} has no runtime smoke record (installed by an older host); \
                         reinstall to verify it can run: {}",
                        install_cmd(pin)
                    )),
                }
            }
            // A worker installed by an older host has no sidecar: not an error,
            // just unknown provenance worth a soft nudge to reinstall.
            None => Some(format!(
                "worker for {pin} has no provenance record (installed by an older host); \
                 reinstall to enable header-drift detection: {}",
                install_cmd(pin)
            )),
        };
        if matches!(window, WindowVerdict::Unknown) {
            return Readiness::UnknownPin {
                pin: pin.as_str().to_owned(),
                worker,
                lean_sysroot,
            };
        }
        Readiness::Ready {
            worker,
            lean_sysroot,
            note,
        }
    }
}

/// `lean-host-mcp install-worker --toolchain <id>` — the exact command that
/// produces a missing or stale worker for `toolchain`.
fn install_cmd(toolchain: &ToolchainId) -> String {
    format!("lean-host-mcp install-worker --toolchain {}", toolchain.as_str())
}

/// Where a pin sits relative to the lean-rs supported window. Pure data
/// derived from [`lean_toolchain::SUPPORTED_TOOLCHAINS`] — see
/// [`ToolchainId::window_verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowVerdict {
    /// Pin matches a known supported toolchain exactly.
    Supported,
    /// Numbered pin (`X.Y.Z[-rcN]`) with no matching entry. `window` is the
    /// inclusive `floor ..= head` range; `nearest` is the closest supported
    /// version to migrate to.
    OutOfWindow { window: String, nearest: String },
    /// Pin does not name a numbered release (e.g. `nightly-*`): allowed, but
    /// the host cannot vouch for it.
    Unknown,
}

impl WindowVerdict {
    /// One-word label for the `install-worker --list` `support` column.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::OutOfWindow { .. } => "outside-window",
            Self::Unknown => "unknown",
        }
    }
}

/// Single verdict for a pinned toolchain at project-open time.
///
/// Produced by [`WorkerBinary::resolve_ready_for`]; each variant maps to
/// exactly one caller outcome (spawn, warn-and-spawn, or a typed
/// `BadProject`).
#[derive(Debug)]
pub enum Readiness {
    /// Spawn the worker. `lean_sysroot` is the toolchain's `elan_dir` (the
    /// child's `LEAN_SYSROOT`); `note` carries a soft advisory (e.g. missing
    /// provenance sidecar) to surface as an envelope warning, `None` when the
    /// worker is fully vouched-for.
    Ready {
        worker: WorkerBinary,
        lean_sysroot: PathBuf,
        note: Option<String>,
    },
    /// Numbered pin outside the supported window. Carries the window range and
    /// the nearest supported version.
    Unsupported { window: String, nearest: String },
    /// The toolchain's `lean.h` changed since the worker was built: rebuild it.
    Stale {
        toolchain: ToolchainId,
        install_cmd: String,
    },
    /// The worker built and its header digest matches, but it failed its
    /// post-build runtime smoke test — the toolchain's `libleanshared` is
    /// ABI-incompatible with this lean-rs build and the worker crashes when it
    /// loads Lean. `detail` is the recorded failure (e.g. `signal: 11
    /// (SIGSEGV)`). A hard verdict: serving it would only produce per-call
    /// `runtime_unavailable` crashes.
    Unusable {
        toolchain: ToolchainId,
        detail: String,
        install_cmd: String,
    },
    /// No worker binary installed for this pin.
    NotInstalled {
        toolchain: ToolchainId,
        install_cmd: String,
    },
    /// The pinned elan toolchain itself is not installed under `~/.elan`.
    ToolchainNotInstalled { toolchain: ToolchainId, elan_dir: PathBuf },
    /// Pin is installed and header-fresh but unrecognized (`nightly-*`):
    /// proceed, but flag it to the caller. Carries `lean_sysroot` so the caller
    /// can spawn just like [`Self::Ready`].
    UnknownPin {
        pin: String,
        worker: WorkerBinary,
        lean_sysroot: PathBuf,
    },
}

/// Sortable key for a Lean version string `X.Y.Z` or `X.Y.Z-rcN`.
///
/// The fourth slot orders release candidates *before* their release
/// (`0` = rc, `1` = release), so `4.31.0-rc1 < 4.31.0`. Returns `None` for
/// anything that is not a numbered release (e.g. `nightly-*`).
fn version_key(s: &str) -> Option<(u32, u32, u32, u8, u32)> {
    let (core, rc) = match s.split_once("-rc") {
        Some((core, rc)) => (core, Some(rc.parse::<u32>().ok()?)),
        None => (s, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(match rc {
        Some(n) => (major, minor, patch, 0, n),
        None => (major, minor, patch, 1, 0),
    })
}

/// Collapse a [`version_key`] into a single monotonic scalar, so version
/// nearness is a difference on a number line rather than a per-component
/// comparison.
///
/// Each field gets a positional weight large enough that a higher field always
/// dominates a lower one (major ≫ minor ≫ patch ≫ rc/release split ≫ rc
/// number). This is what makes "nearest" behave: a pin a whole major above the
/// head is closest to the head (largest scalar), not to whichever lower version
/// happens to share a digit, and `4.30.0-rc1` is closest to its own release
/// `4.30.0` (one rc step, weight ~10³) rather than to `4.29.1` (a patch away,
/// weight ~10⁶). The weights assume the realistic ranges of a Lean version
/// (each field < 1000); see the supported-window table.
fn version_scalar((major, minor, patch, rc_flag, rc_num): (u32, u32, u32, u8, u32)) -> u64 {
    // Saturating throughout (workspace denies unchecked arithmetic); the
    // realistic field ranges are nowhere near `u64::MAX`, so saturation never
    // bites — it just keeps the lint happy and the mapping total.
    u64::from(major)
        .saturating_mul(1_000_000_000_000)
        .saturating_add(u64::from(minor).saturating_mul(1_000_000_000))
        .saturating_add(u64::from(patch).saturating_mul(1_000_000))
        .saturating_add(u64::from(rc_flag).saturating_mul(1_000))
        .saturating_add(u64::from(rc_num))
}

/// The `floor ..= head` window string and the nearest supported version for a
/// numbered pin outside the window. Both derive from
/// [`lean_toolchain::SUPPORTED_TOOLCHAINS`] (ordered ascending; each entry's
/// first version is canonical) — never a hardcoded literal list.
///
/// "Nearest" scans every supported version and picks the one whose key is the
/// smallest [`version_distance`] from the pin, ties broken toward the newer
/// version to bias migration forward. This is why `v4.30.0-rc1` resolves to
/// `4.30.0` (one rc step away) rather than the window floor: the old logic only
/// compared the pin against the head and fell back to the floor for everything
/// below it.
fn out_of_window_bounds(pin: (u32, u32, u32, u8, u32)) -> (String, String) {
    let entries = lean_toolchain::SUPPORTED_TOOLCHAINS;
    let floor = entries
        .first()
        .and_then(|t| t.versions.first())
        .copied()
        .unwrap_or_default();
    let head = entries
        .last()
        .and_then(|t| t.versions.first())
        .copied()
        .unwrap_or_default();
    let window = format!("{floor} ..= {head}");
    let pin_scalar = version_scalar(pin);
    let nearest = entries
        .iter()
        .filter_map(|t| t.versions.first().copied())
        .filter_map(|v| version_key(v).map(|key| (v, version_scalar(key))))
        .min_by(|(_, a), (_, b)| {
            a.abs_diff(pin_scalar)
                .cmp(&b.abs_diff(pin_scalar))
                // Equal distance (pin sits exactly between two releases): prefer
                // the newer supported version, so callers are nudged forward.
                .then_with(|| b.cmp(a))
        })
        .map_or(floor, |(v, _)| v);
    (window, nearest.to_owned())
}

/// Full SHA-256 (lowercase hex) of `<elan_dir>/include/lean/lean.h`. The
/// robust toolchain-identity check: a version string can lie (an rc
/// republished under the same id), the header digest cannot.
pub(crate) fn hash_lean_header(elan_dir: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let path = elan_dir.join("include").join("lean").join("lean.h");
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(digest.len().saturating_mul(2));
    for b in &digest {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// File name of the per-worker provenance sidecar inside
/// `<install_root>/<id>/`.
const SIDECAR_FILE_NAME: &str = "worker.json";

/// Private provenance record written next to an installed worker binary.
///
/// Records what the worker was built against so [`WorkerBinary::resolve_ready_for`]
/// can detect header drift (the toolchain's `lean.h` changing under a worker
/// that keeps being selected). Fields stay private; callers go through the
/// query methods.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkerSidecar {
    toolchain: String,
    /// Full SHA-256 of the `lean.h` the worker was built against.
    header_digest: String,
    /// `lean_toolchain::LEAN_VERSION` the host was built against.
    built_against_lean_version: String,
    /// Whether `supported_by_digest(header_digest)` matched at build time.
    digest_supported_at_build: bool,
    /// Outcome of the post-build runtime smoke test. `None` for a sidecar
    /// written by a host predating the smoke test — unknown, not failed, so the
    /// gate treats it as a soft "reinstall to verify" note rather than a hard
    /// `Unusable`.
    #[serde(default)]
    smoke: Option<SmokeOutcome>,
}

impl WorkerSidecar {
    /// Write `<install_dir>/worker.json` recording `header_digest`, the host's
    /// build-time context, and the post-build `smoke` outcome. Overwrites any
    /// existing record.
    pub(crate) fn record(
        install_dir: &Path,
        id: &ToolchainId,
        header_digest: String,
        smoke: SmokeOutcome,
    ) -> std::io::Result<()> {
        let sidecar = Self {
            toolchain: id.as_str().to_owned(),
            digest_supported_at_build: lean_toolchain::supported_by_digest(&header_digest).is_some(),
            built_against_lean_version: lean_toolchain::LEAN_VERSION.to_owned(),
            header_digest,
            smoke: Some(smoke),
        };
        let json = serde_json::to_string_pretty(&sidecar).map_err(std::io::Error::other)?;
        std::fs::write(install_dir.join(SIDECAR_FILE_NAME), json)
    }

    /// Load `<install_dir>/worker.json`. `None` when absent or unparseable —
    /// an older host left no record, which is unknown provenance, not an error.
    pub(crate) fn load(install_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(install_dir.join(SIDECAR_FILE_NAME)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Whether the recorded build-time digest still matches the toolchain.
    pub(crate) fn header_matches(&self, current_digest: &str) -> bool {
        self.header_digest == current_digest
    }

    /// One-word label for the `install-worker --list` `header` column,
    /// given the current `lean.h` digest (`None` when it could not be read).
    pub(crate) fn header_status(&self, current_digest: Option<&str>) -> &'static str {
        match current_digest {
            Some(current) if self.header_matches(current) => "ok",
            Some(_) => "stale",
            None => "no-record",
        }
    }

    /// The recorded post-build runtime smoke outcome, if any.
    pub(crate) fn smoke(&self) -> Option<&SmokeOutcome> {
        self.smoke.as_ref()
    }

    /// One-word label for the `install-worker --list` `smoke` column
    /// (`passed` / `failed` / `no-record` for a pre-smoke-test sidecar).
    pub(crate) fn smoke_status(&self) -> &'static str {
        self.smoke.as_ref().map_or("no-record", SmokeOutcome::label)
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
            Self::WorkerNotInstalled { toolchain, install_cmd } => {
                write!(f, "no worker binary for toolchain {toolchain}; run: {install_cmd}")
            }
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
            ToolchainId::parse("leanprover/lean4:v4.30.0").unwrap().as_str(),
            "v4.30.0",
        );
        assert_eq!(ToolchainId::parse("v4.30.0").unwrap().as_str(), "v4.30.0",);
        assert_eq!(
            ToolchainId::parse("nightly-2026-05-20").unwrap().as_str(),
            "nightly-2026-05-20",
        );
        assert_eq!(
            ToolchainId::parse("  leanprover/lean4:v4.30.0  \n").unwrap().as_str(),
            "v4.30.0",
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
        fs::write(tmp.path().join("lean-toolchain"), "leanprover/lean4:v4.30.0\n").unwrap();
        let id = ToolchainId::from_lake_root(tmp.path()).unwrap();
        assert_eq!(id.as_str(), "v4.30.0");
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
        let id = ToolchainId::parse("v4.30.0").unwrap();
        let err = WorkerBinary::resolve_with_override(&id, Some(tmp.path())).unwrap_err();
        match err {
            ToolchainError::WorkerNotInstalled { install_cmd, .. } => {
                assert!(install_cmd.contains("v4.30.0"), "got: {install_cmd}");
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
        let id = ToolchainId::parse("v4.30.0").unwrap();
        let nested = tmp.path().join("v4.30.0");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let resolved = WorkerBinary::resolve_with_override(&id, Some(tmp.path())).unwrap();
        assert_eq!(resolved.path, nested.join(WORKER_FILE_NAME));
    }

    #[test]
    fn worker_binary_bare_developer_fallback_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0").unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let resolved = WorkerBinary::resolve_with_override(&id, Some(tmp.path())).unwrap();
        assert_eq!(resolved.path, tmp.path().join(WORKER_FILE_NAME));
    }

    /// The supported window's floor and head, read live from
    /// `SUPPORTED_TOOLCHAINS` so these tests track lean-rs rather than pin a
    /// literal that silently rots.
    fn window_bounds() -> (&'static str, &'static str) {
        let entries = lean_toolchain::SUPPORTED_TOOLCHAINS;
        let floor = entries.first().unwrap().versions.first().unwrap();
        let head = entries.last().unwrap().versions.first().unwrap();
        (floor, head)
    }

    #[test]
    fn window_verdict_accepts_in_window_pin_stripping_leading_v() {
        let (_, head) = window_bounds();
        let id = ToolchainId::parse(&format!("v{head}")).unwrap();
        assert_eq!(id.window_verdict(), WindowVerdict::Supported);
    }

    #[test]
    fn window_verdict_flags_above_head_pin_with_nearest_head() {
        let (floor, head) = window_bounds();
        let major: u32 = head.split('.').next().unwrap().parse().unwrap();
        let id = ToolchainId::parse(&format!("v{}.0.0", major + 1)).unwrap();
        match id.window_verdict() {
            WindowVerdict::OutOfWindow { window, nearest } => {
                assert_eq!(window, format!("{floor} ..= {head}"));
                assert_eq!(nearest, head);
            }
            other @ (WindowVerdict::Supported | WindowVerdict::Unknown) => {
                panic!("expected OutOfWindow, got {other:?}")
            }
        }
    }

    #[test]
    fn window_verdict_flags_below_floor_pin_with_nearest_floor() {
        let (floor, head) = window_bounds();
        let id = ToolchainId::parse("v0.0.0").unwrap();
        match id.window_verdict() {
            WindowVerdict::OutOfWindow { window, nearest } => {
                assert_eq!(window, format!("{floor} ..= {head}"));
                assert_eq!(nearest, floor);
            }
            other @ (WindowVerdict::Supported | WindowVerdict::Unknown) => {
                panic!("expected OutOfWindow, got {other:?}")
            }
        }
    }

    #[test]
    fn window_verdict_flags_in_between_rc_with_nearest_release() {
        // A release candidate of an already-supported release (e.g. `4.30.0-rc1`
        // when `4.30.0` ships) is out of window, and its genuinely-nearest
        // supported version is that release — not the window floor.
        let (floor, _) = window_bounds();
        let release = lean_toolchain::SUPPORTED_TOOLCHAINS
            .iter()
            .filter_map(|t| t.versions.first().copied())
            // A `X.Y.Z` release (no `-rc`) other than the floor, so the rc we
            // synthesize is genuinely between two supported versions.
            .find(|v| !v.contains("-rc") && *v != floor)
            .expect("the supported window should contain a non-floor numbered release");
        let rc = format!("{release}-rc1");
        // Guard: the synthesized rc must not itself be a supported entry.
        assert!(
            lean_toolchain::supported_for(&rc).is_none(),
            "{rc} unexpectedly supported"
        );
        match ToolchainId::parse(&format!("v{rc}")).unwrap().window_verdict() {
            WindowVerdict::OutOfWindow { nearest, .. } => assert_eq!(nearest, release),
            other @ (WindowVerdict::Supported | WindowVerdict::Unknown) => {
                panic!("expected OutOfWindow, got {other:?}")
            }
        }
    }

    #[test]
    fn window_verdict_treats_nightly_as_unknown() {
        let id = ToolchainId::parse("nightly-2026-05-20").unwrap();
        assert_eq!(id.window_verdict(), WindowVerdict::Unknown);
    }

    #[test]
    fn window_string_derives_from_supported_toolchains_not_a_literal() {
        let (floor, head) = window_bounds();
        let WindowVerdict::OutOfWindow { window, .. } = ToolchainId::parse("v0.0.0").unwrap().window_verdict() else {
            panic!("expected OutOfWindow");
        };
        // If the bounds were hardcoded, this would drift when lean-rs bumps.
        assert_eq!(window, format!("{floor} ..= {head}"));
        assert!(window.contains(floor) && window.contains(head));
    }

    #[test]
    fn version_key_orders_rc_before_release() {
        assert!(version_key("4.31.0-rc1") < version_key("4.31.0"));
        assert!(version_key("4.30.0") < version_key("4.31.0-rc1"));
        assert_eq!(version_key("nightly-2026-05-20"), None);
        assert_eq!(version_key("4.31"), None);
    }

    #[test]
    fn sidecar_round_trips_record_then_load() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "abc123".to_owned(), SmokeOutcome::Passed).unwrap();
        let loaded = WorkerSidecar::load(tmp.path()).expect("sidecar should load");
        assert!(loaded.header_matches("abc123"));
        assert!(!loaded.header_matches("different"));
        assert_eq!(loaded.header_status(Some("abc123")), "ok");
        assert_eq!(loaded.header_status(Some("different")), "stale");
        assert_eq!(loaded.header_status(None), "no-record");
        assert_eq!(loaded.smoke_status(), "passed");
        assert_eq!(loaded.smoke(), Some(&SmokeOutcome::Passed));
    }

    #[test]
    fn legacy_sidecar_without_smoke_field_loads_as_no_record() {
        // A sidecar written by a host predating the smoke test has no `smoke`
        // key; `#[serde(default)]` must read it back as unknown, not fail.
        let tmp = tempfile::tempdir().unwrap();
        let legacy = r#"{
            "toolchain": "v4.30.0",
            "header_digest": "abc123",
            "built_against_lean_version": "4.30.0",
            "digest_supported_at_build": true
        }"#;
        fs::write(tmp.path().join(SIDECAR_FILE_NAME), legacy).unwrap();
        let loaded = WorkerSidecar::load(tmp.path()).expect("legacy sidecar should load");
        assert_eq!(loaded.smoke(), None);
        assert_eq!(loaded.smoke_status(), "no-record");
    }

    #[test]
    fn smoke_failed_is_unusable_even_with_matching_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        WorkerSidecar::record(
            tmp.path(),
            &id,
            "digest".to_owned(),
            SmokeOutcome::Failed {
                detail: "signal: 11 (SIGSEGV)".to_owned(),
            },
        )
        .unwrap();
        let sysroot = tmp.path().to_path_buf();
        let readiness = WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, Some("digest"));
        let Readiness::Unusable { detail, .. } = readiness else {
            panic!("expected Unusable, got {readiness:?}");
        };
        assert!(detail.contains("SIGSEGV"), "got: {detail}");
    }

    #[test]
    fn smoke_record_missing_is_ready_with_reinstall_note() {
        // Sidecar present (digest guards drift) but no smoke record: a legacy
        // worker. Ready, but nudged to reinstall to verify it runs.
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let legacy = format!(
            r#"{{"toolchain":"{}","header_digest":"digest","built_against_lean_version":"x","digest_supported_at_build":true}}"#,
            id.as_str()
        );
        fs::write(tmp.path().join(SIDECAR_FILE_NAME), legacy).unwrap();
        let sysroot = tmp.path().to_path_buf();
        let readiness = WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, Some("digest"));
        let Readiness::Ready { note: Some(note), .. } = readiness else {
            panic!("expected Ready with a reinstall note, got {readiness:?}");
        };
        assert!(note.contains("smoke"), "got: {note}");
    }

    #[test]
    fn ready_with_matching_digest_carries_no_note() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "digest".to_owned(), SmokeOutcome::Passed).unwrap();
        let sysroot = tmp.path().to_path_buf();
        let readiness = WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, Some("digest"));
        assert!(
            matches!(readiness, Readiness::Ready { note: None, .. }),
            "expected Ready with no note, got {readiness:?}"
        );
    }

    #[test]
    fn forged_mismatching_digest_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "built-digest".to_owned(), SmokeOutcome::Passed).unwrap();
        let sysroot = tmp.path().to_path_buf();
        assert!(matches!(
            WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, Some("drifted-digest")),
            Readiness::Stale { .. }
        ));
    }

    #[test]
    fn missing_sidecar_is_ready_with_soft_note() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let sysroot = tmp.path().to_path_buf();
        let readiness = WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, Some("whatever"));
        let Readiness::Ready { note: Some(note), .. } = readiness else {
            panic!("expected Ready with a soft note, got {readiness:?}");
        };
        assert!(note.contains("provenance"), "got: {note}");
    }

    #[test]
    fn unknown_nightly_pin_installed_and_fresh_is_unknown_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("nightly-2026-05-20").unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "d".to_owned(), SmokeOutcome::Passed).unwrap();
        let sysroot = tmp.path().to_path_buf();
        assert!(matches!(
            WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, Some("d")),
            Readiness::UnknownPin { .. }
        ));
    }

    #[test]
    fn missing_worker_is_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        let sysroot = tmp.path().to_path_buf();
        assert!(matches!(
            WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, None),
            Readiness::NotInstalled { .. }
        ));
    }

    #[test]
    fn out_of_window_pin_is_unsupported_before_install_check() {
        let tmp = tempfile::tempdir().unwrap();
        // No worker installed, yet the window check fires first.
        let id = ToolchainId::parse("v0.0.0").unwrap();
        let sysroot = tmp.path().to_path_buf();
        assert!(matches!(
            WorkerBinary::resolve_ready_with_override(&id, Some(tmp.path()), sysroot, None),
            Readiness::Unsupported { .. }
        ));
    }
}
