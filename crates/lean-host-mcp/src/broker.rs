//! `ProjectBroker`: the mediator between MCP tool dispatch and
//! [`LeanProject`].
//!
//! Two responsibilities:
//!
//! 1. **Resolve** a tool call's [`ProjectHint`] into a canonical Lake-root
//!    path via the five-step chain
//!    *explicit → env → cwd-walk → config-default → error*.
//! 2. **Lend** an `Arc<LeanProject>` to the tool's closure, opening the
//!    project lazily on first use, reusing it on subsequent calls, and
//!    evicting under LRU and idle pressure.
//!
//! The closure-shaped [`with_project`](ProjectBroker::with_project) API is
//! deliberate: tools never see the registry; they receive a clone of the
//! project's `Arc` and the broker's mutex is released before the closure
//! runs.
//!
//! **LRU + idle eviction.** The registry is an [`lru::LruCache`] capped by
//! [`BrokerConfig::max_projects`] (default 4). A background reaper spawned
//! from [`ProjectBroker::new`] runs every 60 s and evicts entries that have
//! been idle longer than [`BrokerConfig::idle_timeout`] (default 600 s; set
//! to [`Duration::ZERO`] to disable idle eviction).
//!
//! **Manifest invalidation.** Every cache hit re-fingerprints
//! `lake-manifest.json` and treats a mismatch as a miss — the project is
//! shut down and re-spawned. The cost is one ≤ 50 KB SHA-256 per tool call.
//!
//! **Slow-path concurrency.** The registry mutex is never held across
//! [`LeanProject::open`] (multi-second worker spawn) or
//! [`LeanProject::submit`]. Concurrent calls against *different* projects
//! parallelize; concurrent misses for the *same* project race, and the
//! loser's [`LeanProject`] is shut down on the dispatch path.

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use lru::LruCache;
use parking_lot::Mutex;

use crate::error::{Result, ServerError};
use crate::index::fingerprint_lake_project;
use crate::lake_meta::LakeProjectMeta;
use crate::project::LeanProject;

/// Default pool capacity when `LEAN_HOST_MCP_MAX_PROJECTS` is unset.
pub const DEFAULT_MAX_PROJECTS: usize = 4;

/// Default idle-eviction window (10 min) when
/// `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` is unset.
///
/// Long enough that an interactive Claude session keeps its projects warm,
/// short enough that a forgotten project tab releases its worker child
/// within a reasonable bound.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;

const REAPER_TICK: Duration = Duration::from_mins(1);

/// Bag of broker inputs. Built once at startup from the CLI / env /
/// config; `cwd` is injectable so tests can drive the cwd-walk step
/// without `std::env::set_current_dir`.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub cache_dir: PathBuf,
    pub config_default: Option<PathBuf>,
    pub env_default: Option<PathBuf>,
    pub cwd: PathBuf,
    /// Maximum number of [`LeanProject`] instances kept resident. A miss
    /// when the pool is full evicts the LRU entry.
    pub max_projects: NonZeroUsize,
    /// Idle window after which a project is eligible for the reaper.
    /// [`Duration::ZERO`] disables idle eviction (LRU only).
    pub idle_timeout: Duration,
}

impl BrokerConfig {
    /// Parse the two pool-related env vars and return their values, falling
    /// back to [`DEFAULT_MAX_PROJECTS`] / [`DEFAULT_IDLE_TIMEOUT_SECS`].
    ///
    /// `LEAN_HOST_MCP_MAX_PROJECTS = 0` is rejected (it would deadlock the
    /// pool); `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS = 0` disables idle eviction.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when an env var is set but unparseable
    /// (non-numeric, or `MAX_PROJECTS=0`).
    pub fn pool_from_env() -> Result<(NonZeroUsize, Duration)> {
        parse_pool_config(
            std::env::var("LEAN_HOST_MCP_MAX_PROJECTS").ok().as_deref(),
            std::env::var("LEAN_HOST_MCP_IDLE_TIMEOUT_SECS").ok().as_deref(),
        )
    }

    /// Convenience: a [`NonZeroUsize`] with the default pool capacity. Used
    /// by tests, benches, and any caller that just wants the default pool
    /// sizing without rederiving it from [`DEFAULT_MAX_PROJECTS`].
    #[must_use]
    pub fn default_max_projects() -> NonZeroUsize {
        NonZeroUsize::new(DEFAULT_MAX_PROJECTS).unwrap_or(NonZeroUsize::MIN)
    }

    /// Convenience: the default idle window as a [`Duration`].
    #[must_use]
    pub const fn default_idle_timeout() -> Duration {
        Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
    }
}

/// Pure parser shared by [`BrokerConfig::pool_from_env`] and unit tests.
fn parse_pool_config(max: Option<&str>, idle: Option<&str>) -> Result<(NonZeroUsize, Duration)> {
    let max_projects = match max {
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|e| ServerError::Internal(format!("LEAN_HOST_MCP_MAX_PROJECTS={s:?} not a usize: {e}")))?;
            NonZeroUsize::new(n)
                .ok_or_else(|| ServerError::Internal("LEAN_HOST_MCP_MAX_PROJECTS=0 would deadlock the pool".into()))?
        }
        None => BrokerConfig::default_max_projects(),
    };
    let idle_timeout = match idle {
        Some(s) => {
            let n: u64 = s.parse().map_err(|e| {
                ServerError::Internal(format!("LEAN_HOST_MCP_IDLE_TIMEOUT_SECS={s:?} not a u64: {e}"))
            })?;
            Duration::from_secs(n)
        }
        None => BrokerConfig::default_idle_timeout(),
    };
    Ok((max_projects, idle_timeout))
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

struct BrokerInner {
    registry: LruCache<PathBuf, Arc<LeanProject>>,
    last_used: HashMap<PathBuf, Instant>,
}

pub struct ProjectBroker {
    inner: Mutex<BrokerInner>,
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
    /// Construct a broker and spawn the background idle reaper.
    ///
    /// The reaper is only spawned when a tokio runtime is available
    /// ([`tokio::runtime::Handle::try_current`]) and `idle_timeout > 0`.
    /// Sync test contexts get a working broker without a reaper, which is
    /// fine because tests call [`Self::reap_idle`] directly.
    #[must_use]
    pub fn new(config: BrokerConfig) -> Arc<Self> {
        let broker = Arc::new(Self {
            inner: Mutex::new(BrokerInner {
                registry: LruCache::new(config.max_projects),
                last_used: HashMap::new(),
            }),
            config,
        });
        broker.spawn_reaper();
        broker
    }

    fn spawn_reaper(self: &Arc<Self>) {
        if self.config.idle_timeout.is_zero() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<Self> = Arc::downgrade(self);
        handle.spawn(async move {
            let mut tick = tokio::time::interval(REAPER_TICK);
            // First tick fires immediately; skip it so the test contract
            // (`reap_idle` is the only thing that can evict synchronously)
            // is undisturbed.
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(broker) = weak.upgrade() else {
                    break;
                };
                broker.reap_idle();
            }
        });
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
    /// mutex is released before `job` runs, and is never held across
    /// [`LeanProject::open`] on the miss path.
    ///
    /// # Errors
    ///
    /// Resolution failures, manifest-read failures, and
    /// [`LeanProject::open`] failures travel as [`ServerError`]. The
    /// closure's own errors propagate unchanged.
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

    /// Look up or open the project for `root`. Mutex is released around
    /// the slow path so concurrent calls for different roots parallelize.
    fn acquire(&self, root: PathBuf) -> Result<Arc<LeanProject>> {
        // Fast path: registry hit + matching manifest hash.
        let cached = {
            let mut inner = self.inner.lock();
            inner.registry.get(&root).cloned()
        };
        if let Some(project) = cached {
            let current_hash = fingerprint_lake_project(&root)?;
            if project.manifest_hash() == current_hash {
                self.inner.lock().last_used.insert(root, Instant::now());
                return Ok(project);
            }
            // Stale entry; evict and fall through to reopen.
            let stale = {
                let mut inner = self.inner.lock();
                inner.last_used.remove(&root);
                inner.registry.pop(&root)
            };
            if let Some(s) = stale {
                s.shutdown();
            }
            drop(project);
        }

        // Slow path: open without holding the lock.
        let meta = LakeProjectMeta::from_explicit(&root)?;
        let opened = LeanProject::open(meta, &self.config.cache_dir)?;

        // Reacquire, race-resolve, insert with possible eviction.
        let mut inner = self.inner.lock();
        let (project, victim) = if let Some(existing) = inner.registry.get(&root).cloned() {
            // Someone else won the race. Use theirs; our `opened` will
            // shut down when the local `Arc` drops.
            inner.last_used.insert(root, Instant::now());
            (existing, Some(opened))
        } else {
            let victim = if inner.registry.len() >= inner.registry.cap().get() {
                inner.registry.pop_lru()
            } else {
                None
            };
            if let Some((ref evicted_path, _)) = victim {
                inner.last_used.remove(evicted_path);
            }
            inner.registry.put(root.clone(), Arc::clone(&opened));
            inner.last_used.insert(root, Instant::now());
            (opened, victim.map(|(_, v)| v))
        };
        drop(inner);
        if let Some(v) = victim {
            v.shutdown();
        }
        Ok(project)
    }

    /// Evict every entry whose `last_used` timestamp is older than
    /// `config.idle_timeout`. Called every [`REAPER_TICK`] by the
    /// background task; tests call this directly to avoid waiting.
    pub fn reap_idle(&self) {
        if self.config.idle_timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        let evicted: Vec<Arc<LeanProject>> = {
            let mut inner = self.inner.lock();
            let expired: Vec<PathBuf> = inner
                .last_used
                .iter()
                .filter(|(_, last)| now.saturating_duration_since(**last) >= self.config.idle_timeout)
                .map(|(p, _)| p.clone())
                .collect();
            let mut out: Vec<Arc<LeanProject>> = Vec::with_capacity(expired.len());
            for p in &expired {
                if let Some(proj) = inner.registry.pop(p) {
                    out.push(proj);
                }
                inner.last_used.remove(p);
            }
            out
        };
        for v in evicted {
            v.shutdown();
        }
    }

    /// Snapshot of paths currently resident in the pool, ordered MRU-first.
    /// Public so integration tests can assert eviction without going
    /// through a tool call.
    #[must_use]
    pub fn resident_paths(&self) -> Vec<PathBuf> {
        let inner = self.inner.lock();
        inner.registry.iter().map(|(p, _)| p.clone()).collect()
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
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: Duration::ZERO,
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
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "explicit");
        let broker = ProjectBroker::new(cfg(tmp.path().to_path_buf(), None, None));
        let resolved = broker.resolve(&ProjectHint::Explicit(proj.clone())).unwrap();
        assert_eq!(resolved, proj);
    }

    #[test]
    fn parse_pool_config_uses_defaults_when_unset() {
        let (max, idle) = parse_pool_config(None, None).unwrap();
        assert_eq!(max.get(), DEFAULT_MAX_PROJECTS);
        assert_eq!(idle, Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS));
    }

    #[test]
    fn parse_pool_config_accepts_explicit_values() {
        let (max, idle) = parse_pool_config(Some("8"), Some("30")).unwrap();
        assert_eq!(max.get(), 8);
        assert_eq!(idle, Duration::from_secs(30));
    }

    #[test]
    fn parse_pool_config_treats_zero_idle_as_disable() {
        let (_, idle) = parse_pool_config(None, Some("0")).unwrap();
        assert_eq!(idle, Duration::ZERO);
    }

    #[test]
    fn parse_pool_config_rejects_max_projects_zero() {
        let err = parse_pool_config(Some("0"), None).unwrap_err();
        assert!(matches!(err, ServerError::Internal(_)), "{err:?}");
    }

    #[test]
    fn parse_pool_config_rejects_garbage() {
        assert!(parse_pool_config(Some("seven"), None).is_err());
        assert!(parse_pool_config(None, Some("forever")).is_err());
    }
}
