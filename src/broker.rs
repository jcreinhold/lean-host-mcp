//! `ProjectBroker`: the mediator between MCP tool dispatch and
//! [`LeanProject`].
//!
//! Two responsibilities:
//!
//! 1. **Resolve** a tool call's [`ProjectHint`] into a canonical Lake-root
//!    path via the five-step chain
//!    *explicit → env → cwd-walk → config-default → error*.
//! 2. **Lend** an `Arc<LeanProject>` to the tool's closure, opening the
//!    project lazily on first use and reusing it on subsequent calls.
//!
//! The closure-shaped [`with_project`](ProjectBroker::with_project) API is
//! deliberate: the broker's internal registry is a `HashMap<PathBuf,
//! Arc<LeanProject>>`, single-entry today but multi-entry-shaped so the
//! tool surface doesn't move when an LRU policy lands in a follow-up.
//! Tools never see the registry; they receive a clone of the `Arc` and
//! the broker's mutex is released before the closure runs.
//!
//! **Single-entry invariant (this iteration):** switching the resolved
//! root drops the existing `LeanProject` and reopens against the new
//! root. The hot-swap is wasteful for repeated cross-project hops but
//! keeps the API shape stable; eviction policy is a downstream concern.
//!
//! **Mutex held across `LeanProject::open`:** opening a project spawns
//! the worker child (multi-second). The registry mutex is held for that
//! duration because there is no concurrency model in this iteration —
//! concurrent tool calls against the same broker are not yet expected.
//! When concurrency lands, the open will move outside the lock.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{Result, ServerError};
use crate::lake_meta::LakeProjectMeta;
use crate::project::LeanProject;

/// Bag of broker inputs. Built once at startup from the CLI / env /
/// config; `cwd` is injectable so tests can drive the cwd-walk step
/// without `std::env::set_current_dir`.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub cache_dir: PathBuf,
    pub config_default: Option<PathBuf>,
    pub env_default: Option<PathBuf>,
    pub cwd: PathBuf,
}

/// Per-call routing hint. `Default` runs the full resolution chain;
/// `Explicit` short-circuits to the supplied path.
#[derive(Debug, Clone)]
pub enum ProjectHint {
    Explicit(PathBuf),
    Default,
}

impl ProjectHint {
    /// Build a hint from a tool request's optional `project` field.
    /// `None`/empty string falls through to [`Self::Default`].
    #[must_use]
    pub fn from_request(value: Option<String>) -> Self {
        match value {
            Some(s) if !s.is_empty() => Self::Explicit(PathBuf::from(s)),
            _ => Self::Default,
        }
    }
}

pub struct ProjectBroker {
    registry: Mutex<HashMap<PathBuf, Arc<LeanProject>>>,
    config: BrokerConfig,
}

impl std::fmt::Debug for ProjectBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectBroker")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProjectBroker {
    #[must_use]
    pub fn new(config: BrokerConfig) -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(HashMap::new()),
            config,
        })
    }

    /// Apply the resolution chain. Public so callers can pre-flight the
    /// resolution without opening a project.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::BadProject`] when the explicit / env /
    /// config path is unusable or no lakefile is reachable from `cwd`.
    pub fn resolve(&self, hint: &ProjectHint) -> Result<PathBuf> {
        if let ProjectHint::Explicit(p) = hint {
            return canonicalise(p);
        }
        if let Some(env) = &self.config.env_default {
            return canonicalise(env);
        }
        if let Some(found) = walk_up_for_lakefile(&self.config.cwd) {
            return canonicalise(&found);
        }
        if let Some(cfg) = &self.config.config_default {
            return canonicalise(cfg);
        }
        Err(ServerError::BadProject(format!(
            "no lakefile found from {} and no env/config default set",
            self.config.cwd.display()
        )))
    }

    /// Resolve the hint, ensure a [`LeanProject`] is open for that root,
    /// and run `job` with a clone of the project's `Arc`. The registry
    /// mutex is released before `job` runs.
    ///
    /// # Errors
    ///
    /// Resolution failures and `LeanProject::open` failures travel as
    /// [`ServerError::BadProject`] / [`ServerError::Lean`] /
    /// [`ServerError::Index`] / [`ServerError::Internal`]. The closure's
    /// own errors propagate unchanged.
    pub async fn with_project<F, Fut, R>(&self, hint: ProjectHint, job: F) -> Result<R>
    where
        F: FnOnce(Arc<LeanProject>) -> Fut + Send,
        Fut: Future<Output = Result<R>> + Send,
        R: Send,
    {
        let root = self.resolve(&hint)?;
        let project = self.acquire(root)?;
        job(project).await
    }

    /// Look up or open the project for `root`. Holds the registry mutex
    /// across [`LeanProject::open`] — see module docs.
    fn acquire(&self, root: PathBuf) -> Result<Arc<LeanProject>> {
        let project = {
            let mut registry = self.registry.lock();
            if let Some(existing) = registry.get(&root) {
                Arc::clone(existing)
            } else {
                let meta = LakeProjectMeta::from_explicit(&root)?;
                let opened = LeanProject::open(meta, &self.config.cache_dir)?;
                // Single-entry invariant: drop any prior project before
                // inserting. The prior `Arc` is released here; the actor
                // thread shuts down when the last `Arc` drops.
                registry.clear();
                registry.insert(root, Arc::clone(&opened));
                opened
            }
        };
        Ok(project)
    }
}

fn canonicalise(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|e| ServerError::BadProject(format!("canonicalise {}: {e}", path.display())))
}

/// Walk upward from `start` looking for `lakefile.toml` / `lakefile.lean`.
/// Returns the directory containing the lakefile.
fn walk_up_for_lakefile(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        if dir.join("lakefile.toml").is_file() || dir.join("lakefile.lean").is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test code uses unwrap/panic to surface failure paths concisely"
)]
mod tests {
    use std::fs;

    use super::*;

    fn make_lake_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("lakefile.lean"),
            format!("package {name}\nlean_lib {}\n", name.replace('-', "_")),
        )
        .unwrap();
        dir.canonicalize().unwrap()
    }

    fn cfg(cwd: PathBuf, env: Option<PathBuf>, conf: Option<PathBuf>) -> BrokerConfig {
        BrokerConfig {
            cache_dir: std::env::temp_dir(),
            config_default: conf,
            env_default: env,
            cwd,
        }
    }

    #[test]
    fn walk_up_for_lakefile_finds_in_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "myproj");
        let nested = proj.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(walk_up_for_lakefile(&nested).unwrap(), proj);
    }

    #[test]
    fn resolve_returns_canonicalised_path_when_no_resolution_needed() {
        // Sanity: canonicalise is exercised on every branch.
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "explicit");
        let broker = ProjectBroker::new(cfg(tmp.path().to_path_buf(), None, None));
        let resolved = broker.resolve(&ProjectHint::Explicit(proj.clone())).unwrap();
        assert_eq!(resolved, proj);
    }
}
