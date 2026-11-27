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
//!   the corresponding Lean shared library. It lives in the worker directory
//!   [`workers_dir`] picks for a Lake root, at
//!   `<dir>/<id>/lean-host-mcp-worker` (or a bare `<dir>/lean-host-mcp-worker`
//!   for a developer build).
//! - [`ToolchainError`] is the typed failure surface. Project-open code
//!   maps it into [`crate::error::ServerError::BadProject`] with the
//!   [`install_fix`] command, so the fix flows through to the client.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config_file::{self, ConfigFile};
use crate::smoke::SmokeOutcome;

/// Env var that names elan's home, as elan itself reads it.
pub const ELAN_HOME_ENV: &str = "ELAN_HOME";

/// The `[install] elan_home` of the Lake root this process serves, set by
/// [`use_recorded_elan_home`].
static RECORDED_ELAN_HOME: OnceLock<PathBuf> = OnceLock::new();

/// Look for elan in `path` when `ELAN_HOME` is unset.
///
/// `path` is the `[install] elan_home` that `install` recorded for the Lake
/// root this process serves. Call it once, before any toolchain lookup.
///
/// # Errors
///
/// Fails when an earlier call recorded another path.
pub fn use_recorded_elan_home(path: &Path) -> io::Result<()> {
    if RECORDED_ELAN_HOME.get_or_init(|| path.to_path_buf()) == path {
        Ok(())
    } else {
        Err(io::Error::other(
            "the recorded elan home is already set to another path",
        ))
    }
}

/// Elan's home, where the Lean toolchains are.
///
/// It is `ELAN_HOME`, then the recorded `[install] elan_home`, then
/// `$HOME/.elan`. `install`, `install-worker` and the server all resolve it
/// here, and set it as `ELAN_HOME` on the Lean and Lake processes they start.
#[must_use]
pub fn elan_home() -> Option<PathBuf> {
    elan_home_from(
        std::env::var_os(ELAN_HOME_ENV),
        RECORDED_ELAN_HOME.get().map(PathBuf::as_path),
        dirs::home_dir(),
    )
}

/// Set `ELAN_HOME` on `command` to [`elan_home`], so the Lean, Lake or Cargo
/// process it starts finds the same elan whatever its `HOME`.
pub(crate) fn pass_elan_home(command: &mut std::process::Command) {
    if let Some(home) = elan_home() {
        command.env(ELAN_HOME_ENV, home);
    }
}

/// [`elan_home`] with its inputs passed in. An empty `ELAN_HOME` counts as unset.
pub(crate) fn elan_home_from(env: Option<OsString>, recorded: Option<&Path>, home: Option<PathBuf>) -> Option<PathBuf> {
    env.filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| recorded.map(Path::to_path_buf))
        .or_else(|| home.map(|home| home.join(".elan")))
}

/// Canonical short form of a Lean toolchain pin (e.g. `v4.30.0`,
/// `nightly-2026-05-20`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolchainId(String);

/// File name of the per-toolchain worker binary inside a worker directory.
pub const WORKER_FILE_NAME: &str = "lean-host-mcp-worker";

/// Env var that overrides the worker directory for every Lake root, for
/// example to serve a worker built in this checkout. It beats the config
/// files' `[install] workers_dir`.
pub const WORKERS_DIR_ENV: &str = "LEAN_HOST_MCP_WORKERS_DIR";

/// The worker directory for Lake root `root`: `flag` (a CLI flag), then
/// `LEAN_HOST_MCP_WORKERS_DIR`, then `[install] workers_dir` from the root's
/// config files, then `<root>/.local/lean-host-mcp/workers`.
///
/// `install`, `install-worker` and the server all pick the directory here, so
/// the server looks where install put the worker.
#[must_use]
pub fn workers_dir(root: &Path, flag: Option<&Path>) -> PathBuf {
    workers_dir_with(root, flag, std::env::var_os(WORKERS_DIR_ENV))
}

/// [`workers_dir`] with the env var's value passed in.
pub(crate) fn workers_dir_with(root: &Path, flag: Option<&Path>, env: Option<OsString>) -> PathBuf {
    config_file::pick_dir(
        flag,
        env,
        || ConfigFile::load(Some(root)).install.workers_dir(root),
        || config_file::local_dir(root).join("workers"),
    )
}

/// The command that installs what Lake root `root` needs to serve:
/// `<this executable> install --lake-root <root>`, quoted for a POSIX shell.
#[must_use]
pub fn install_fix(root: &Path) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lean-host-mcp"));
    format!(
        "{} install --lake-root {}",
        shell_word(&exe.to_string_lossy()),
        shell_word(&root.to_string_lossy())
    )
}

/// `word` as one POSIX shell word: unchanged when it holds only safe
/// characters, else single-quoted.
pub(crate) fn shell_word(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

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
    /// (`<elan home>/toolchains/leanprover--lean4---<id>`, with the elan home
    /// from [`elan_home`]).
    ///
    /// # Errors
    ///
    /// [`ToolchainError::ElanToolchainNotInstalled`] if the directory is
    /// absent. Returns the path even when present so callers can build
    /// further on top of it.
    pub fn elan_dir(&self) -> Result<PathBuf, ToolchainError> {
        let name = format!("leanprover--lean4---{}", self.0);
        let Some(home) = elan_home() else {
            return Err(ToolchainError::ElanToolchainNotInstalled {
                toolchain: self.clone(),
                elan_dir: Path::new("$ELAN_HOME/toolchains").join(name),
            });
        };
        let dir = home.join("toolchains").join(name);
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

    /// Total-order key for presenting toolchains in a sensible order: numbered
    /// releases sorted semantically (a release candidate sorts *before* its
    /// release, e.g. `v4.31.0-rc1 < v4.31.0`), then non-numbered pins
    /// (`nightly-*`) after, ordered by their string (dates sort naturally).
    ///
    /// Reuses `version_key` so the ordering matches the supported-window
    /// logic. Callers (`install-worker` listing/scan) `.cmp()` these keys
    /// instead of comparing the raw strings, whose lexical order wrongly puts
    /// `v4.31.0-rc1` *after* `v4.31.0` (the release is a prefix of the rc).
    #[must_use]
    pub fn sort_key(&self) -> (u8, (u32, u32, u32, u8, u32), String) {
        let bare = self.0.strip_prefix('v').unwrap_or(&self.0);
        match version_key(bare) {
            Some(k) => (0, k, String::new()),
            None => (1, (0, 0, 0, 0, 0), self.0.clone()),
        }
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
    /// worker does not exist yet, so the full [`WorkerBinary::resolve_ready`]
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
    /// Look up the worker binary for `toolchain` in the worker directory
    /// `dir`: a bare developer build at `<dir>/lean-host-mcp-worker` wins,
    /// else the selected generation of `<dir>/<id>/`.
    ///
    /// # Errors
    ///
    /// [`ToolchainError::WorkerNotInstalled`] when no candidate exists.
    pub fn resolve_in(toolchain: &ToolchainId, dir: &Path) -> Result<Self, ToolchainError> {
        Self::resolve_location(toolchain, dir).map(|(worker, _)| worker)
    }

    fn resolve_location(toolchain: &ToolchainId, dir: &Path) -> Result<(Self, bool), ToolchainError> {
        let not_installed = || ToolchainError::WorkerNotInstalled {
            toolchain: toolchain.clone(),
            dir: dir.to_path_buf(),
        };
        let slot = if dir.join(WORKER_FILE_NAME).is_file() {
            dir.to_path_buf()
        } else {
            dir.join(toolchain.as_str())
        };
        let generation = Self::selected_dir(&slot).map_err(|_| not_installed())?;
        let path = generation.join(WORKER_FILE_NAME);
        if !path.is_file() {
            return Err(not_installed());
        }
        Ok((
            Self {
                path,
                toolchain: toolchain.clone(),
            },
            generation != slot,
        ))
    }

    /// Resolve the generation once. A malformed selector must not revive an old binary.
    pub(crate) fn selected_dir(slot: &Path) -> std::io::Result<PathBuf> {
        use std::io::Read as _;
        let selector = slot.join("current-generation");
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid installed worker generation");
        match std::fs::symlink_metadata(&selector) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(slot.to_owned()),
            Ok(meta) if meta.is_file() && meta.len() <= 80 => {}
            Err(e) => return Err(e),
            Ok(_) => return Err(invalid()),
        }
        let mut file = std::fs::File::open(&selector)?;
        let mut name = String::new();
        file.by_ref().take(81).read_to_string(&mut name)?;
        if name.len() > 80
            || !name
                .strip_prefix("generation-")
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            return Err(invalid());
        }
        let path = slot.join(name);
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            return Err(invalid());
        }
        Ok(path)
    }

    /// The one answer project-open needs about a pinned toolchain: window
    /// membership, elan + worker install presence, and header-digest
    /// provenance, folded into a single [`Readiness`] verdict the caller maps
    /// to one outcome (spawn, warn-and-spawn, or a typed `BadProject`).
    ///
    /// This hides five independently-volatile decisions behind one call — the
    /// window source ([`ToolchainId::window_verdict`]), the elan layout
    /// ([`ToolchainId::elan_dir`]), the worker install layout
    /// ([`Self::resolve_in`]), the header-digest provenance
    /// mechanism, and the recorded runtime smoke result (both via the private
    /// `WorkerSidecar`) — so no call site consults a classifier and a separate
    /// provenance check. An out-of-window pin short-circuits before any
    /// filesystem probe, so the caller gets the window message even when the
    /// bogus toolchain was never installed.
    ///
    /// On the happy path it hashes `<elan_dir>/include/lean/lean.h` once (a
    /// few-KB SHA-256); this belongs on the cold resolve/open path, not the
    /// warm per-call path. `dir` is the worker directory and `fix` the command
    /// that soft advisories name.
    #[must_use]
    pub fn resolve_ready(pin: &ToolchainId, dir: &Path, fix: &str) -> Readiness {
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
        Self::resolve_ready_with(pin, dir, fix, elan_dir, current.as_deref())
    }

    /// [`Self::resolve_ready`] with the resolved `lean_sysroot` (the
    /// toolchain's `elan_dir`) and the already-computed current `lean.h`
    /// digest injected rather than read from the filesystem. The test suite
    /// drives the gate's window/install/provenance logic through this seam
    /// without a real toolchain on disk.
    #[must_use]
    pub fn resolve_ready_with(
        pin: &ToolchainId,
        dir: &Path,
        fix: &str,
        lean_sysroot: PathBuf,
        current_digest: Option<&str>,
    ) -> Readiness {
        // Self-contained for direct test callers: re-check the window so the
        // seam alone produces the full verdict set.
        let window = pin.window_verdict();
        if let WindowVerdict::OutOfWindow { window, nearest } = window {
            return Readiness::Unsupported { window, nearest };
        }
        let Ok((worker, selected_generation)) = Self::resolve_location(pin, dir) else {
            return Readiness::NotInstalled { toolchain: pin.clone() };
        };
        // Provenance, in order of severity:
        //   1. header drift     → Stale
        //   2. protocol mismatch → Incompatible
        //   3. smoke failed      → Unusable
        //   4. smoke missing     → Ready + soft note
        //   5. sidecar absent    → Ready + soft note
        let install_dir = worker.path.parent().unwrap_or(&worker.path);
        let sidecar = WorkerSidecar::load(install_dir);
        if selected_generation {
            let valid = sidecar.as_ref().is_some_and(|record| {
                record.toolchain == pin.as_str()
                    && matches!(record.smoke(), Some(SmokeOutcome::Passed))
                    && record.deployment.as_ref().is_some_and(|deployment| {
                        deployment
                            .runtime
                            .as_ref()
                            .is_some_and(|runtime| runtime.sysroot == lean_sysroot)
                            && std::fs::symlink_metadata(&worker.path).is_ok_and(|meta| meta.is_file())
                            && crate::cli::install_worker::sha256_prefix(&worker.path, 64)
                                .is_ok_and(|digest| digest == deployment.worker_sha256)
                    })
            });
            if !valid {
                return Readiness::Unusable {
                    toolchain: pin.clone(),
                    detail: "selected worker generation is incomplete or differs from its installed deployment".into(),
                };
            }
        }
        let note = match sidecar {
            Some(sidecar) => {
                // A recorded build-time digest that no longer matches the
                // toolchain's current lean.h means the header drifted under the
                // worker; a rebuild is the right move regardless of any smoke
                // verdict, so check it first.
                if let Some(current) = current_digest
                    && !sidecar.header_matches(current)
                {
                    return Readiness::Stale { toolchain: pin.clone() };
                }
                if let Some(worker_protocol) = sidecar.protocol_version()
                    && worker_protocol != lean_rs_worker_protocol::protocol::PROTOCOL_VERSION
                {
                    return Readiness::Incompatible {
                        toolchain: pin.clone(),
                        worker_protocol,
                        host_protocol: lean_rs_worker_protocol::protocol::PROTOCOL_VERSION,
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
                        };
                    }
                    Some(SmokeOutcome::Passed) => None,
                    // Built by an older host that did not smoke-test: the header
                    // digest still guards drift, but the worker is unverified at
                    // runtime — nudge a reinstall to record a smoke result.
                    None => Some(format!(
                        "worker for {pin} has no runtime smoke record (installed by an older host); \
                         reinstall to verify it can run: {fix}"
                    )),
                }
            }
            // A worker installed by an older host has no sidecar: not an error,
            // just unknown provenance worth a soft nudge to reinstall.
            None => Some(format!(
                "worker for {pin} has no provenance record (installed by an older host); \
                 reinstall to enable header-drift detection: {fix}"
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
    /// One-word label for the `install-worker --list` `support` column:
    /// `supported` (lean-rs supports this version), `unsupported` (a numbered
    /// release outside the window), or `unknown` (not a numbered release, e.g.
    /// a nightly).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::OutOfWindow { .. } => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// Single verdict for a pinned toolchain at project-open time.
///
/// Produced by [`WorkerBinary::resolve_ready`]; each variant maps to
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
    Stale { toolchain: ToolchainId },
    /// The installed worker records a different wire protocol. Refuse it
    /// before spawn and provide the exact reinstall action.
    Incompatible {
        toolchain: ToolchainId,
        worker_protocol: u16,
        host_protocol: u16,
    },
    /// The worker built and its header digest matches, but it failed its
    /// post-build runtime smoke test — the toolchain's `libleanshared` is
    /// ABI-incompatible with this lean-rs build and the worker crashes when it
    /// loads Lean. `detail` is the recorded failure (e.g. `signal: 11
    /// (SIGSEGV)`). A hard verdict: serving it would only produce per-call
    /// `runtime_unavailable` crashes.
    Unusable { toolchain: ToolchainId, detail: String },
    /// No worker binary installed for this pin.
    NotInstalled { toolchain: ToolchainId },
    /// The pinned elan toolchain itself is not installed under the elan home.
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
/// happens to share a digit, and `4.31.0-rc1` is closest to its own release
/// `4.31.0` (one rc step, weight ~10³) rather than to `4.30.0` (a minor away,
/// weight ~10⁹). The weights assume the realistic ranges of a Lean version
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
/// "Nearest" scans every supported version and picks the one whose
/// [`version_scalar`] lies closest to the pin's, ties broken toward the newer
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
    crate::digest::file_sha256_hex(&elan_dir.join("include").join("lean").join("lean.h"))
}

/// File name of the per-worker provenance sidecar inside
/// `<workers_dir>/<id>/`.
const SIDECAR_FILE_NAME: &str = "worker.json";

/// Private provenance record written next to an installed worker binary.
///
/// Records what the worker was built against so [`WorkerBinary::resolve_ready`]
/// can detect header drift (the toolchain's `lean.h` changing under a worker
/// that keeps being selected). Fields stay private; callers go through the
/// query methods.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkerSidecar {
    toolchain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deployment: Option<WorkerDeployment>,
    /// Full SHA-256 of the `lean.h` the worker was built against.
    header_digest: String,
    /// `lean_toolchain::LEAN_VERSION` the host was built against.
    built_against_lean_version: String,
    /// The `lean-host-mcp` package version that built this worker. Informational:
    /// compatibility is determined by `worker_protocol_version`, not `SemVer`.
    #[serde(default)]
    built_by_host_version: String,
    /// `lean-rs-worker-protocol` wire version. Zero for a legacy sidecar; the
    /// runtime handshake remains authoritative for those workers.
    #[serde(default)]
    worker_protocol_version: u16,
    /// Whether `supported_by_digest(header_digest)` matched at build time.
    digest_supported_at_build: bool,
    /// Outcome of the post-build runtime smoke test. `None` for a sidecar
    /// written by a host predating the smoke test — unknown, not failed, so the
    /// gate treats it as a soft "reinstall to verify" note rather than a hard
    /// `Unusable`.
    #[serde(default)]
    smoke: Option<SmokeOutcome>,
}

/// Candidate executable binding. This alone grants no native operation.
#[derive(serde::Serialize, serde::Deserialize)]
struct WorkerDeployment {
    worker_sha256: String,
    #[serde(flatten)]
    runtime: Option<InstalledRuntime>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct InstalledRuntime {
    pub(crate) sysroot: PathBuf,
    pub(crate) prepared: lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts,
    pub(crate) files: std::collections::BTreeMap<PathBuf, String>,
}

impl InstalledRuntime {
    /// Convert between installed paths and SDK inputs at the generation boundary.
    pub(crate) fn map_generation_paths(
        &mut self,
        map: impl Fn(&Path) -> std::io::Result<PathBuf>,
    ) -> std::io::Result<()> {
        let string = |value: &mut String| -> std::io::Result<()> {
            *value = map(Path::new(value))?
                .into_os_string()
                .into_string()
                .map_err(|_| std::io::Error::other("non-UTF8 shim path"))?;
            Ok(())
        };
        let p = &mut self.prepared;
        string(&mut p.shim_manifest_path)?;
        for path in p.shim_search_paths.iter_mut().chain(&mut p.artifact_roots) {
            string(path)?;
        }
        self.files = std::mem::take(&mut self.files)
            .into_iter()
            .map(|(path, hash)| {
                if path.starts_with(&self.sysroot) {
                    Ok((path, hash))
                } else {
                    Ok((map(&path)?, hash))
                }
            })
            .collect::<std::io::Result<_>>()?;
        Ok(())
    }
}

/// Non-builtin modules used to bootstrap the installed one-shot batch verifier.
pub const VERIFIER_MODULES: &[&str] = &[
    "LeanRsHostShims.InfoTree",
    "LeanRsHostShims.Elaboration",
    "LeanRsHostShims.Environment",
    "LeanRsInterop.Callback",
    "LeanRsInterop.Callback.String",
    "LeanRsInterop.Callback.Tick",
];

impl WorkerSidecar {
    pub(crate) fn into_runtime(self) -> Option<InstalledRuntime> {
        self.deployment?.runtime
    }

    /// Write `<install_dir>/worker.json` recording `header_digest`, the host's
    /// build-time context, and the post-build `smoke` outcome. Overwrites any
    /// existing record.
    #[cfg(test)]
    pub(crate) fn record(
        install_dir: &Path,
        id: &ToolchainId,
        header_digest: String,
        smoke: SmokeOutcome,
    ) -> std::io::Result<()> {
        let mut sidecar = Self::candidate(id, header_digest, None);
        sidecar.smoke = Some(smoke);
        sidecar.write(install_dir)
    }

    pub(crate) fn candidate(id: &ToolchainId, header_digest: String, worker_sha256: Option<String>) -> Self {
        Self {
            deployment: worker_sha256.map(|worker_sha256| WorkerDeployment {
                worker_sha256,
                runtime: None,
            }),
            toolchain: id.as_str().to_owned(),
            digest_supported_at_build: lean_toolchain::supported_by_digest(&header_digest).is_some(),
            built_against_lean_version: lean_toolchain::LEAN_VERSION.to_owned(),
            built_by_host_version: env!("CARGO_PKG_VERSION").to_owned(),
            worker_protocol_version: lean_rs_worker_protocol::protocol::PROTOCOL_VERSION,
            header_digest,
            smoke: None,
        }
    }

    pub(crate) fn bind_runtime(&mut self, runtime: InstalledRuntime) -> std::io::Result<()> {
        let deployment = self
            .deployment
            .as_mut()
            .ok_or_else(|| std::io::Error::other("missing candidate worker identity"))?;
        deployment.runtime = Some(runtime);
        Ok(())
    }

    pub(crate) fn complete(&mut self, digest: &str, smoke: SmokeOutcome) -> std::io::Result<()> {
        if self.deployment.as_ref().is_none_or(|d| d.worker_sha256 != digest) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "candidate worker changed during smoke",
            ));
        }
        self.smoke = Some(smoke);
        Ok(())
    }

    pub(crate) fn write(&self, install_dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(install_dir.join(SIDECAR_FILE_NAME), json)
    }

    /// Load `<install_dir>/worker.json`. `None` when absent or unparseable —
    /// an older host left no record, which is unknown provenance, not an error.
    pub(crate) fn load(install_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(install_dir.join(SIDECAR_FILE_NAME)).ok()?;
        let mut record: Self = serde_json::from_slice(&bytes).ok()?;
        if let Some(runtime) = record.deployment.as_mut().and_then(|d| d.runtime.as_mut())
            && Path::new(&runtime.prepared.shim_manifest_path).is_relative()
        {
            runtime
                .map_generation_paths(|path| {
                    if !path.starts_with("shims")
                        || path.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
                    {
                        return Err(std::io::Error::other("invalid generation-relative path"));
                    }
                    Ok(install_dir.join(path))
                })
                .ok()?;
        }
        Some(record)
    }

    /// Whether the recorded build-time digest still matches the toolchain.
    pub(crate) fn header_matches(&self, current_digest: &str) -> bool {
        self.header_digest == current_digest
    }

    /// Recorded worker wire protocol, or `None` for a legacy sidecar.
    pub(crate) fn protocol_version(&self) -> Option<u16> {
        (self.worker_protocol_version != 0).then_some(self.worker_protocol_version)
    }

    /// One-word protocol compatibility label for `install-worker --list`.
    pub(crate) fn protocol_status(&self) -> &'static str {
        match self.protocol_version() {
            None => "unknown",
            Some(version) if version == lean_rs_worker_protocol::protocol::PROTOCOL_VERSION => "compatible",
            Some(_) => "incompatible",
        }
    }

    /// One-word label for the `install-worker --list` `build` column, given the
    /// toolchain's current `lean.h` digest (`None` when it could not be read, so
    /// freshness cannot be judged): `fresh` (build still matches its toolchain),
    /// `stale` (the header drifted under it — rebuild), or `unknown`.
    pub(crate) fn header_status(&self, current_digest: Option<&str>) -> &'static str {
        match current_digest {
            Some(current) if self.header_matches(current) => "fresh",
            Some(_) => "stale",
            None => "unknown",
        }
    }

    /// The recorded post-build runtime smoke outcome, if any.
    pub(crate) fn smoke(&self) -> Option<&SmokeOutcome> {
        self.smoke.as_ref()
    }

    /// One-word label for the `install-worker --list` `runtime` column:
    /// `runs` / `crashed`, or `untested` for a sidecar written before the
    /// post-build smoke test existed.
    pub(crate) fn smoke_status(&self) -> &'static str {
        self.smoke.as_ref().map_or("untested", SmokeOutcome::label)
    }
}

/// Typed failures during toolchain resolution. Project-open code maps
/// these into [`crate::error::ServerError::BadProject`].
#[derive(Debug)]
pub enum ToolchainError {
    UnparseableToolchainString(String),
    LeanToolchainFileMissing(PathBuf),
    ElanToolchainNotInstalled { toolchain: ToolchainId, elan_dir: PathBuf },
    WorkerNotInstalled { toolchain: ToolchainId, dir: PathBuf },
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
            Self::WorkerNotInstalled { toolchain, dir } => {
                write!(f, "no worker for leanprover/lean4:{toolchain} in {}", dir.display())
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

    /// The fix command the readiness tests pass; notes must repeat it.
    const FIX: &str = "lean-host-mcp install --lake-root /project";

    #[test]
    fn elan_home_prefers_the_env_then_the_recorded_path_then_home() {
        let env = || Some(OsString::from("/env/elan"));
        let recorded = Some(Path::new("/recorded/elan"));
        let home = || Some(PathBuf::from("/home/user"));
        assert_eq!(
            elan_home_from(env(), recorded, home()),
            Some(PathBuf::from("/env/elan"))
        );
        assert_eq!(
            elan_home_from(None, recorded, home()),
            Some(PathBuf::from("/recorded/elan"))
        );
        assert_eq!(
            elan_home_from(Some(OsString::new()), recorded, home()),
            Some(PathBuf::from("/recorded/elan")),
            "an empty ELAN_HOME counts as unset"
        );
        assert_eq!(
            elan_home_from(None, None, home()),
            Some(PathBuf::from("/home/user/.elan"))
        );
        assert_eq!(elan_home_from(None, None, None), None);
    }

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
    fn missing_worker_names_the_toolchain_and_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0").unwrap();
        let err = WorkerBinary::resolve_in(&id, tmp.path()).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("no worker for leanprover/lean4:v4.30.0 in {}", tmp.path().display())
        );
    }

    #[test]
    fn workers_dir_reads_the_lake_root_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let pick = |flag: Option<&Path>| workers_dir_with(root, flag, None);
        assert_eq!(pick(None), root.join(".local/lean-host-mcp/workers"));
        fs::write(root.join("lean-host-mcp.toml"), "[install]\nworkers_dir = 'tracked'\n").unwrap();
        assert_eq!(pick(None), root.join("tracked"));
        config_file::ensure_local_dir(root).unwrap();
        fs::write(
            config_file::local_file(root),
            "[install]\nworkers_dir = '/local/workers'\n",
        )
        .unwrap();
        assert_eq!(pick(None), Path::new("/local/workers"));
        assert_eq!(
            workers_dir_with(root, None, Some("/env".into())),
            Path::new("/env"),
            "the env var beats both files"
        );
        assert_eq!(pick(Some(Path::new("/flag"))), Path::new("/flag"));
    }

    #[test]
    fn install_fix_is_one_shell_command() {
        let fix = install_fix(Path::new("/work/my project"));
        assert!(fix.ends_with(" install --lake-root '/work/my project'"), "got: {fix}");
        assert_eq!(shell_word("/plain/path-1.2"), "/plain/path-1.2");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }

    #[test]
    fn worker_binary_with_id_subdir_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0").unwrap();
        let nested = tmp.path().join("v4.30.0");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let resolved = WorkerBinary::resolve_in(&id, tmp.path()).unwrap();
        assert_eq!(resolved.path, nested.join(WORKER_FILE_NAME));
    }

    #[test]
    fn worker_binary_bare_developer_fallback_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0").unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let resolved = WorkerBinary::resolve_in(&id, tmp.path()).unwrap();
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
    fn window_verdict_flags_in_between_patch_with_nearest_release() {
        // A patch above an interior supported release (e.g. `4.31.1` when the
        // window holds `4.31.0`) is out of window, and its genuinely-nearest
        // supported version is that release — not the window floor or head.
        //
        // The witness is a patch gap rather than an rc of a supported release
        // because the current window pairs every interior numbered release
        // with its rc (`4.31.0-rc1`, `4.32.0-rc1` are supported entries), so
        // no rc witness exists; the patch gap pins the same nearest-scan.
        let (floor, _) = window_bounds();
        let release = lean_toolchain::SUPPORTED_TOOLCHAINS
            .iter()
            .filter_map(|t| t.versions.first().copied())
            // A `X.Y.Z` release (no `-rc`) other than the floor, so the patch
            // we synthesize is genuinely between two supported versions.
            .find(|v| !v.contains("-rc") && *v != floor)
            .expect("the supported window should contain a non-floor numbered release");
        let mut parts = release.split('.');
        let (major, minor, patch) = (
            parts.next().expect("numbered release has a major"),
            parts.next().expect("numbered release has a minor"),
            parts.next().expect("numbered release has a patch"),
        );
        let next_patch = patch.parse::<u32>().expect("numbered release patch is a number") + 1;
        let gap = format!("{major}.{minor}.{next_patch}");
        // Guard: the synthesized patch must not itself be a supported entry.
        assert!(
            lean_toolchain::supported_for(&gap).is_none(),
            "{gap} unexpectedly supported"
        );
        match ToolchainId::parse(&format!("v{gap}")).unwrap().window_verdict() {
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
    fn sort_key_orders_rc_before_release() {
        let rc = ToolchainId::parse("v4.31.0-rc1").unwrap();
        let rel = ToolchainId::parse("v4.31.0").unwrap();
        let rc2 = ToolchainId::parse("v4.31.0-rc2").unwrap();
        let prev = ToolchainId::parse("v4.30.0").unwrap();
        let ngt = ToolchainId::parse("nightly-2026-05-20").unwrap();
        assert!(rc.sort_key() < rel.sort_key(), "rc before its release");
        assert!(rc.sort_key() < rc2.sort_key(), "rc1 before rc2");
        assert!(prev.sort_key() < rc.sort_key(), "older release before the next rc");
        assert!(rel.sort_key() < ngt.sort_key(), "numbered release before a nightly");

        // The lexical order this replaces would sort to [prev, rel, rc, rc2].
        let mut ids = vec![rel.clone(), prev.clone(), rc2.clone(), rc.clone()];
        ids.sort_by_key(ToolchainId::sort_key);
        assert_eq!(ids, vec![prev, rc, rc2, rel]);
    }

    #[test]
    fn sidecar_round_trips_record_then_load() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "abc123".to_owned(), SmokeOutcome::Passed).unwrap();
        let loaded = WorkerSidecar::load(tmp.path()).expect("sidecar should load");
        assert!(loaded.header_matches("abc123"));
        assert!(!loaded.header_matches("different"));
        assert_eq!(loaded.header_status(Some("abc123")), "fresh");
        assert_eq!(loaded.header_status(Some("different")), "stale");
        assert_eq!(loaded.header_status(None), "unknown");
        assert_eq!(loaded.smoke_status(), "runs");
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
        assert_eq!(loaded.smoke_status(), "untested");
    }

    #[test]
    fn selected_generation_requires_the_installed_worker_before_admission() {
        let root = tempfile::tempdir().unwrap();
        let pin = ToolchainId::parse("v4.35.0").unwrap();
        let slot = root.path().join(pin.as_str());
        let generation = slot.join("generation-example");
        fs::create_dir_all(&generation).unwrap();
        fs::write(slot.join("current-generation"), "generation-example").unwrap();
        let worker = generation.join(WORKER_FILE_NAME);
        fs::write(&worker, b"qualified worker").unwrap();
        let ready = || WorkerBinary::resolve_ready_with(&pin, root.path(), FIX, root.path().to_owned(), Some("header"));
        assert!(matches!(ready(), Readiness::Unusable { .. }));
        let digest = crate::cli::install_worker::sha256_prefix(&worker, 64).unwrap();
        let mut record = WorkerSidecar::candidate(&pin, "header".into(), Some(digest.clone()));
        record
            .bind_runtime(InstalledRuntime {
                sysroot: root.path().to_owned(),
                prepared: lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts {
                    protocol: 1,
                    shim_manifest_path: "shims/test".into(),
                    shim_manifest_json: "{}".into(),
                    artifact_roots: vec![],
                    shim_search_paths: vec![],
                },
                files: std::collections::BTreeMap::new(),
            })
            .unwrap();
        record.complete(&digest, SmokeOutcome::Passed).unwrap();
        record.write(&generation).unwrap();
        assert!(matches!(ready(), Readiness::Ready { .. }));
        fs::write(&worker, b"different worker").unwrap();
        assert!(matches!(ready(), Readiness::Unusable { .. }));
        fs::write(&worker, b"qualified worker").unwrap();
        assert!(matches!(ready(), Readiness::Ready { .. }));
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
        let readiness = WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, Some("digest"));
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
        let readiness = WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, Some("digest"));
        let Readiness::Ready { note: Some(note), .. } = readiness else {
            panic!("expected Ready with a reinstall note, got {readiness:?}");
        };
        assert!(note.contains("smoke") && note.ends_with(FIX), "got: {note}");
    }

    #[test]
    fn ready_with_matching_digest_carries_no_note() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "digest".to_owned(), SmokeOutcome::Passed).unwrap();
        let sysroot = tmp.path().to_path_buf();
        let readiness = WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, Some("digest"));
        assert!(
            matches!(readiness, Readiness::Ready { note: None, .. }),
            "expected Ready with no note, got {readiness:?}"
        );
    }

    #[test]
    fn protocol_version_round_trips_and_legacy_sidecar_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("v4.30.0").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "abc".to_owned(), SmokeOutcome::Passed).unwrap();
        let loaded = WorkerSidecar::load(tmp.path()).unwrap();
        assert_eq!(
            loaded.protocol_version(),
            Some(lean_rs_worker_protocol::protocol::PROTOCOL_VERSION)
        );
        assert_eq!(loaded.protocol_status(), "compatible");

        // A sidecar written before the field existed reads back as unknown
        // provenance (serde default ""), never a spurious mismatch.
        let legacy = tempfile::tempdir().unwrap();
        let json = r#"{"toolchain":"v4.30.0","header_digest":"abc","built_against_lean_version":"x","digest_supported_at_build":true,"smoke":{"result":"passed"}}"#;
        fs::write(legacy.path().join(SIDECAR_FILE_NAME), json).unwrap();
        let old = WorkerSidecar::load(legacy.path()).unwrap();
        assert_eq!(old.protocol_version(), None);
        assert_eq!(old.protocol_status(), "unknown");
    }

    #[test]
    fn package_version_skew_is_ready_when_protocol_is_compatible() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let skewed = format!(
            r#"{{"toolchain":"{}","header_digest":"digest","built_against_lean_version":"x","built_by_host_version":"0.0.1-old","worker_protocol_version":{},"digest_supported_at_build":true,"smoke":{{"result":"passed"}}}}"#,
            id.as_str(),
            lean_rs_worker_protocol::protocol::PROTOCOL_VERSION
        );
        fs::write(tmp.path().join(SIDECAR_FILE_NAME), skewed).unwrap();
        let sysroot = tmp.path().to_path_buf();
        let readiness = WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, Some("digest"));
        assert!(
            matches!(readiness, Readiness::Ready { note: None, .. }),
            "expected compatible package skew to be silent, got {readiness:?}"
        );
    }

    #[test]
    fn protocol_mismatch_is_a_hard_incompatibility() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let skewed = format!(
            r#"{{"toolchain":"{}","header_digest":"digest","built_against_lean_version":"x","built_by_host_version":"0.0.1-old","worker_protocol_version":999,"digest_supported_at_build":true,"smoke":{{"result":"passed"}}}}"#,
            id.as_str()
        );
        fs::write(tmp.path().join(SIDECAR_FILE_NAME), skewed).unwrap();
        let readiness =
            WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, tmp.path().to_path_buf(), Some("digest"));
        assert!(matches!(
            readiness,
            Readiness::Incompatible {
                worker_protocol: 999,
                ..
            }
        ));
    }

    #[test]
    fn forged_mismatching_digest_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "built-digest".to_owned(), SmokeOutcome::Passed).unwrap();
        let sysroot = tmp.path().to_path_buf();
        assert!(matches!(
            WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, Some("drifted-digest")),
            Readiness::Stale { .. }
        ));
    }

    #[test]
    fn missing_sidecar_is_ready_with_soft_note() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        let sysroot = tmp.path().to_path_buf();
        let readiness = WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, Some("whatever"));
        let Readiness::Ready { note: Some(note), .. } = readiness else {
            panic!("expected Ready with a soft note, got {readiness:?}");
        };
        assert!(note.contains("provenance") && note.ends_with(FIX), "got: {note}");
    }

    #[test]
    fn unknown_nightly_pin_installed_and_fresh_is_unknown_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse("nightly-2026-05-20").unwrap();
        fs::write(tmp.path().join(WORKER_FILE_NAME), b"#!/bin/sh\n").unwrap();
        WorkerSidecar::record(tmp.path(), &id, "d".to_owned(), SmokeOutcome::Passed).unwrap();
        let sysroot = tmp.path().to_path_buf();
        assert!(matches!(
            WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, Some("d")),
            Readiness::UnknownPin { .. }
        ));
    }

    #[test]
    fn missing_worker_is_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ToolchainId::parse(&format!("v{}", window_bounds().1)).unwrap();
        let sysroot = tmp.path().to_path_buf();
        assert!(matches!(
            WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, None),
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
            WorkerBinary::resolve_ready_with(&id, tmp.path(), FIX, sysroot, None),
            Readiness::Unsupported { .. }
        ));
    }
}
