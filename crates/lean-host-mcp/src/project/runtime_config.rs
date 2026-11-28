//! Runtime policy for project actors: the import residue budget and the Lean
//! heap ceiling derived from it, the request deadline, mailbox depth and the
//! restart breaker. Each knob resolves env var over config file over a
//! built-in default, once at server startup.

use std::time::Duration;

use crate::config_file::{self, Env, RuntimeFileConfig};
use crate::error::{Result, ServerError};

/// Room the sizing rules leave for the one import that may be in flight.
///
/// `q_max = 4.51 GB`, the largest single-import residue measured over
/// kan-proofs, rounded up: a budget with less than this left over on the machine
/// cannot afford the import that would fill it.
const WORKER_IMPORT_HEADROOM_BYTES: u64 = 4608 * 1024 * 1024;
/// Least gap [`lean_max_memory_kib_for`] leaves between the residue budget and
/// the heap ceiling, whatever the budget.
///
/// 8 GiB against a measured 4.4–6.3 GiB offset. The margin is deliberate: the
/// offset comes from one project's imports, and being too low aborts a healthy
/// child while being too high only defers to the OS.
const WORKER_HEAP_HEADROOM_FLOOR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Lean *heap* ceiling for a worker child, enforced inside the child by
/// `lean_internal_set_max_memory` — derived from the residue budget rather than
/// fixed, because the two are not independent.
///
/// **This is a suicide switch, not a graceful limit.** It was documented as
/// surfacing a Lean-domain failure inside the `ok` payload; that is false in
/// this embedding, and measurably so. Lean's allocator signals the overrun by
/// throwing a C++ exception, which unwinds into the Rust frame at the shim FFI
/// boundary and gets `fatal runtime error: Rust cannot catch foreign
/// exceptions, aborting` — exit status 134, the whole child. Lean's *monadic*
/// check point (`Core.checkMaxMemory`) does raise a catchable error, but only
/// where Lean code reaches it; `importModules` never does. Observed on a
/// three-file kan-proofs sweep at the old fixed 8 GiB: files 1 and 2 answered,
/// file 3 aborted the child mid-import.
///
/// Which is why the value cannot be a constant. Crossing this ceiling kills the
/// child, so it must sit above everything healthy work reaches — and healthy
/// work reaches the residue budget by construction, because that is the point at
/// which the supervisor recycles cleanly. A fixed 8 GiB sat *below* the 9 GiB
/// budget floor, so at Mathlib scale the suicide switch always preempted the
/// clean recycle and the entire residue policy was unreachable. Deriving it
/// keeps the order structural: budget first, abort only if something is wrong.
///
/// The gap has to cover what the *heap* counts and the budget does not, and both
/// terms below are needed because that quantity is neither constant nor
/// proportional. In the observed abort the heap crossed 8 GiB while residue was
/// between 1.71 and 3.62 GiB, so the offset — the elaborator's own heap, the
/// first import's non-residue allocation, and the transient peak inside
/// `importModules` — was 4.4–6.3 GiB. That offset tracks the *project*, not the
/// budget, so a purely proportional gap is too small at a low budget; and it is
/// measured on one project, so a purely fixed gap would be too small for a
/// heavier one. Take whichever binds:
///
/// - [`WORKER_HEAP_HEADROOM_FLOOR_BYTES`] covers the offset outright, which is
///   what a budget below it needs.
/// - Doubling covers a project whose offset scales past that floor, which is
///   what a budget above it needs.
///
/// The error is also asymmetric, so both terms err high. Too low aborts healthy
/// work — the bug this fixes. Too high means the OS reaps the child instead,
/// which for a *child* process is nearly the same outcome, minus the thrash. The
/// one thing this really asserts is that the clean recycle wins.
const fn lean_max_memory_kib_for(import_residue_budget_bytes: u64) -> u64 {
    let headroom = if import_residue_budget_bytes > WORKER_HEAP_HEADROOM_FLOOR_BYTES {
        import_residue_budget_bytes
    } else {
        WORKER_HEAP_HEADROOM_FLOOR_BYTES
    };
    import_residue_budget_bytes.saturating_add(headroom) / 1024
}
/// Depth of one project's job queue, and the only admission mechanism in the
/// server. The actor thread runs one job at a time, so this bounds how many
/// callers may be waiting on a single worker before the project sheds load
/// retryably rather than queueing without limit. Sized to the process-wide
/// waiter bound the deleted semantic-admission semaphore used to enforce, now
/// applied per project — different projects no longer contend for one budget.
const PROJECT_MAILBOX_CAPACITY: usize = 16;
/// Per-request worker deadline. Covers one tool call end to end (live rows,
/// diagnostics, terminal response); on expiry the worker is recycled and the
/// call returns a retryable runtime error. Replaces the worker-parent's 10-min
/// `long_running_requests` profile, which let whole-project scans (e.g.
/// `find_references` at project scope) appear to hang. Raise it for unusually
/// heavy modules whose `verify`/`proof_state` legitimately runs longer.
const REQUEST_TIMEOUT_MILLIS: u64 = 120 * 1000;
/// How many bytes of unreclaimable import residue one worker child may retain
/// before the supervisor cycles it — the ceiling on the RAM-derived default.
///
/// This is the **entire** memory policy for import residue, and the *only* one
/// that recycles cleanly. [`lean_max_memory_kib_for`] derives a heap ceiling
/// above it, but crossing that ceiling aborts the child, so it is a backstop
/// against a residue-accounting bug rather than a second policy.
///
/// The two are not independent, and believing they were is what put the ceiling
/// below the budget. Only the *first* `importModules` in a process maps its
/// compacted regions; every later one re-materialises them as private copies,
/// which are ordinary heap allocations the ceiling does see. So the same bytes
/// this counts are most of what accumulates against the heap ceiling.
///
/// Session reuse removed growth with call count, but not with import count: a
/// Lean environment imported with `loadExts := true` cannot be reclaimed
/// (`Environment.freeRegions` is unsound there), so every import a child
/// performs is retained for the life of that child even after its session is
/// dropped. What this bound counts is those retained bytes directly — the
/// `non_memory_mapped_region_bytes` Lean attributes to each import, summed over
/// the child's generation.
///
/// **Why bytes and not a count.** This was `WORKER_MAX_IMPORTS = 4` until the
/// unit was measured. On `~/Code/kan-proofs` with real per-file import headers,
/// four imports cost 9.60 GiB of process `phys_footprint` on one sample of
/// profiles and 16.00 GiB on another — the same count, 1.7× the memory, on the
/// same machine and the same project. An import ranges from tens of megabytes to
/// several gigabytes depending on its closure, so a count that is right for one
/// project is wrong for the next by two orders of magnitude.
///
/// **What this buys, and what it does not.** It buys scale-independence, which
/// depends on no measurement: `4` recycles a 50 MB/import project after 200 MB
/// for nothing, and a 2 GB/import project after 8 GB whether or not the machine
/// cares. It does **not** buy fewer restarts at Mathlib scale. Residue there is
/// ~2–4.5 GB per import — the profile's whole closure, because `.olean`
/// compacted regions are position-dependent and only the *first* `importModules`
/// in a process maps them at their preferred addresses; every later import
/// re-materialises even shared modules as private copies. Against the floor
/// below that is three or four imports per generation, which is roughly where
/// the count already sat. Restarts at that scale are physics: *k* files means
/// ~*k* imports, and residue that cannot be reclaimed must be recycled every
/// `budget / residue-per-import` imports whatever any policy says. What Part 3's
/// idle cycling changes is *when* that cost lands, not how often.
///
/// A workload that repeats one import profile — the proof loop this server
/// exists to serve — never trips it, because a reused session is not an import
/// and charges nothing (`Response::HostSessionReused` leaves the accumulator
/// untouched). Nor does one cycling among at most
/// [`WORKER_SESSION_POOL_CAPACITY`] profiles: returning to a pooled profile is a
/// key comparison.
///
/// This is deliberately *not* an RSS threshold, and the reason is stronger than
/// it used to be. RSS counts shared, clean, mmapped `.olean` pages, so an
/// RSS-valued limit fires immediately on a Mathlib-scale project. Worse, it is
/// anti-correlated: across twelve real imports ΔRSS/Δfootprint was 5.8 on the
/// first and **0.00** after — RSS *fell* while retained memory grew by 2.3–5.4
/// GB, because the OS evicts exactly the clean pages RSS was counting. The
/// quantity here is Lean's own attribution of what it could not map, read off a
/// value the child already computes, needing no `ps` fork.
const WORKER_IMPORT_RESIDUE_CEILING_BYTES: u64 = 12 * 1024 * 1024 * 1024;
/// The floor under the derived residue budget, and the load-bearing half of it.
///
/// Below one import's residue the policy degenerates to "cycle before every
/// import", which is strictly worse than the count bound it replaces. `2 × q_max`
/// where `q_max = 4.51 GB` is the largest single-import residue measured over
/// kan-proofs, so a generation always fits at least two imports and the session
/// pool has something to do.
const WORKER_IMPORT_RESIDUE_FLOOR_BYTES: u64 = 9 * 1024 * 1024 * 1024;
/// The budget a machine too small for the floor gets instead.
///
/// Degenerate on purpose — one import per generation — because a single import
/// can never restart before itself: the accumulator only advances on completed
/// imports and the bound is tested before the request. So this is "recycle after
/// every import", which is well defined, safe, and the most such a machine can
/// be given. Nonzero so the MiB round-trip through `[runtime]` stays nonzero.
const WORKER_IMPORT_RESIDUE_MIN_BYTES: u64 = 1024 * 1024;
/// Reciprocal of the share of system RAM all workers' residue budgets may
/// claim, before division by `BrokerConfig::max_workers`.
///
/// A quarter: residue is the part of a child that cannot be reclaimed, not its
/// whole footprint, and the OS needs the rest.
const WORKER_IMPORT_RESIDUE_RAM_DIVISOR: u64 = 4;
/// Assumed system RAM when the platform will not report it.
const WORKER_ASSUMED_SYSTEM_RAM_BYTES: u64 = 24 * 1024 * 1024 * 1024;
/// Percentage of the residue budget at which an *idle* child is cycled.
///
/// The hard bound stops a request; this one fires between requests, where the
/// ~730 ms respawn costs the client nothing.
const WORKER_IMPORT_RESIDUE_SOFT_PERCENT: u64 = 60;
/// How many imported environments one worker child pools.
///
/// No longer the same number as the restart bound, and their being equal was a
/// bug: it made the child's LRU eviction unreachable, because the parent cycled
/// the child at exactly the point the pool would have begun evicting. They price
/// different things. A held environment costs ~70 MiB — 8,396,800 KiB holding
/// five against 8,110,080 KiB dropping each, measured on kan-proofs — while the
/// import it saves costs 2.0–4.5 GB. Holding is 1.6–3.5% of importing, so
/// capacity should track the workload's working set of distinct profiles, not
/// the memory bound.
///
/// `8` because one [`crate::tools::semantic::lean_verify`] call checks each file
/// its target groups name, and median PR diffs run 3–12 files; at 4, a 5-file
/// call thrashes the pool inside one tool call. Eight costs ~560 MiB held
/// against 8 GB or more of re-imports avoided.
const WORKER_SESSION_POOL_CAPACITY: usize = 8;
const MAX_RESTARTS_PER_WINDOW: usize = 3;
pub(super) const RESTART_WINDOW: Duration = Duration::from_mins(1);

/// Runtime policy for one private project actor.
///
/// The binary parses this once at server startup and passes it into the
/// broker. Tests and embedders can construct the default directly without
/// rereading process environment during project open.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectRuntimeConfig {
    pub(super) lean_max_memory_kib: u64,
    pub(super) request_timeout_millis: u64,
    pub(super) mailbox_capacity: usize,
    pub(super) max_restarts_per_window: usize,
    pub(super) restart_window: Duration,
    pub(super) import_residue_budget_bytes: u64,
    pub(super) session_pool_capacity: usize,
}

impl Default for ProjectRuntimeConfig {
    fn default() -> Self {
        Self::default_for_workers(crate::broker::DEFAULT_MAX_WORKERS)
    }
}

impl ProjectRuntimeConfig {
    /// Defaults for a broker that runs up to `max_workers` worker children.
    ///
    /// The residue budget and the heap ceiling derived from it are per child,
    /// but every child draws on the same RAM, so they are sized from one
    /// child's share of the machine. Deriving them from the compiled-in pool
    /// size while the operator had configured another was the bug this
    /// replaces: four children each sized as if alone.
    #[must_use]
    pub fn default_for_workers(max_workers: usize) -> Self {
        let import_residue_budget_bytes = default_import_residue_budget_bytes(max_workers);
        Self {
            lean_max_memory_kib: lean_max_memory_kib_for(import_residue_budget_bytes),
            request_timeout_millis: REQUEST_TIMEOUT_MILLIS,
            mailbox_capacity: PROJECT_MAILBOX_CAPACITY,
            max_restarts_per_window: MAX_RESTARTS_PER_WINDOW,
            restart_window: RESTART_WINDOW,
            import_residue_budget_bytes,
            session_pool_capacity: WORKER_SESSION_POOL_CAPACITY,
        }
    }

    /// Configuration advisories belong to broker startup, not every worker reopen.
    pub(crate) fn log_advisories(&self) {
        if self.import_residue_budget_bytes < WORKER_IMPORT_RESIDUE_FLOOR_BYTES {
            tracing::debug!(
                budget_mib = self.import_residue_budget_bytes / (1024 * 1024),
                sizing_baseline_mib = WORKER_IMPORT_RESIDUE_FLOOR_BYTES / (1024 * 1024),
                "configured import residue budget is below the two-import Mathlib sizing estimate; \
             this is supported, not a detected memory problem"
            );
        }
        // Only reachable when an operator sets `runtime.lean_max_memory_kib`
        // explicitly — the derived default cannot invert. Worth a warning rather
        // than a clamp because it is their machine and their call, but it is almost
        // never what they meant: below the budget the ceiling always fires first,
        // and it fires as a child abort rather than a clean recycle.
        if self.lean_max_memory_kib.saturating_mul(1024) <= self.import_residue_budget_bytes {
            tracing::warn!(
                lean_max_memory_mib = self.lean_max_memory_kib / 1024,
                budget_mib = self.import_residue_budget_bytes / (1024 * 1024),
                "Lean heap ceiling is at or below the import residue budget, so it will abort the \
             child before the budget can recycle it cleanly; raise runtime.lean_max_memory_kib \
             above the budget or leave it unset to track it"
            );
        }
    }
}

/// The residue budget one worker gets when nothing overrides it.
///
/// Derived rather than constant because the quantity it bounds is a share of the
/// machine: up to `max_workers` children run at once, and residue is the part
/// of a child that cannot be handed back. The floor is what actually decides the value on any machine
/// smaller than about 150 GiB, which is deliberate — see
/// [`WORKER_IMPORT_RESIDUE_FLOOR_BYTES`]. Sizing below one import's residue
/// would be strictly worse than the count bound this replaces, so the fraction
/// is allowed to lose.
fn default_import_residue_budget_bytes(max_workers: usize) -> u64 {
    import_residue_budget_for(
        system_ram_bytes().unwrap_or(WORKER_ASSUMED_SYSTEM_RAM_BYTES),
        max_workers,
    )
}

/// The policy half of [`default_import_residue_budget_bytes`], split out from
/// the one platform read so the sizing rules can be tested on machines that do
/// not exist.
fn import_residue_budget_for(ram: u64, max_workers: usize) -> u64 {
    let workers = max_workers.max(1) as u64;
    let per_worker = ram
        .checked_div(WORKER_IMPORT_RESIDUE_RAM_DIVISOR)
        .and_then(|share| share.checked_div(workers))
        .unwrap_or(WORKER_IMPORT_RESIDUE_FLOOR_BYTES);
    // The floor is unconditional, and on any machine under ~12 GiB it exceeds
    // the whole machine — a budget the child cannot reach before the OS kills it
    // is not a budget, and the `SIGKILL` it fails to prevent is the exact
    // failure this subsystem exists for. Cap it at one child's share of the
    // machine, less the one import that may be in flight when the bound is
    // tested: the budget is per child, but every resident child draws on the
    // same RAM, and a pool of four does not make the machine four times
    // larger. On an undersized machine — or a roomy one asked to run too many
    // workers — the result lands below the floor, which is honest: that is
    // what `actor_main`'s below-floor warning is there to say.
    let affordable = ram
        .checked_div(workers)
        .unwrap_or(0)
        .saturating_sub(WORKER_IMPORT_HEADROOM_BYTES)
        .max(WORKER_IMPORT_RESIDUE_MIN_BYTES);
    per_worker
        .clamp(WORKER_IMPORT_RESIDUE_FLOOR_BYTES, WORKER_IMPORT_RESIDUE_CEILING_BYTES)
        .min(affordable)
}

/// Total physical RAM, or `None` where the platform will not say.
///
/// Read once at startup and never again: the number does not change, and this is
/// a sizing input rather than a live signal. Deliberately not a dependency — one
/// `sysctl` and one `/proc` read are the whole implementation.
#[cfg(target_os = "macos")]
fn system_ram_bytes() -> Option<u64> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn system_ram_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    kib.checked_mul(1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn system_ram_bytes() -> Option<u64> {
    None
}

impl ProjectRuntimeConfig {
    /// Parse runtime env vars once at server startup.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when a runtime env var is malformed, zero
    /// where zero is unsafe.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with_file(&RuntimeFileConfig::default(), crate::broker::DEFAULT_MAX_WORKERS)
    }

    /// Resolve the runtime policy with a config-file section as the layer
    /// beneath env vars: each knob is `env var > file > built-in default`.
    ///
    /// `max_workers` is the broker's configured pool size. The residue budget
    /// and the heap ceiling derived from it are per child, so their defaults
    /// are one child's share of the machine, not the share a pool of
    /// [`crate::broker::DEFAULT_MAX_WORKERS`] would get.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when an env var is malformed, or a resolved
    /// value (from env or file) is zero where zero is unsafe.
    pub fn from_env_with_file(file: &RuntimeFileConfig, max_workers: usize) -> Result<Self> {
        parse_runtime_config(&config_file::process_env, file, max_workers)
    }

    /// Unreclaimable import residue one worker child may retain before the
    /// supervisor cycles it. The derived default is a share of host RAM,
    /// clamped to at most 12 GiB.
    #[must_use]
    pub const fn import_residue_budget_bytes(&self) -> u64 {
        self.import_residue_budget_bytes
    }

    /// Override the residue budget on an already-resolved config.
    ///
    /// The one knob an embedder that bypasses [`Self::from_env`] still has to
    /// be able to set: every other default is a policy constant that holds on
    /// any machine, while this one is a share of the host's RAM and the default
    /// can only guess at how many workers will actually be resident. Also how
    /// a test forces the residue path to fire without a Mathlib-scale import.
    ///
    /// Moves the Lean heap ceiling with it (see [`Self::lean_max_memory_kib`]):
    /// a ceiling below the budget aborts the child where the budget would have
    /// recycled it cleanly. Callers do not get to hold the two independently,
    /// because there is no correct way to.
    #[must_use]
    pub const fn with_import_residue_budget_bytes(mut self, bytes: u64) -> Self {
        self.import_residue_budget_bytes = bytes;
        self.lean_max_memory_kib = lean_max_memory_kib_for(bytes);
        self
    }

    /// The residue at which an *idle* child is cycled proactively, so the
    /// respawn lands between requests rather than inside one.
    #[must_use]
    pub const fn import_residue_soft_bytes(&self) -> u64 {
        self.import_residue_budget_bytes
            .saturating_div(100)
            .saturating_mul(WORKER_IMPORT_RESIDUE_SOFT_PERCENT)
    }

    /// How many imported environments one worker child pools (default 8). A
    /// held environment costs tens of MiB while re-importing one costs
    /// gigabytes, so a small pool pays for itself.
    #[must_use]
    pub const fn session_pool_capacity(&self) -> usize {
        self.session_pool_capacity
    }

    /// The Lean heap ceiling applied to each worker child, in KiB: the import
    /// residue budget plus headroom equal to the larger of the budget and
    /// 8 GiB. Kept above the budget so an over-budget child is recycled cleanly
    /// by the supervisor instead of aborted by Lean.
    #[must_use]
    pub const fn lean_max_memory_kib(&self) -> u64 {
        self.lean_max_memory_kib
    }

    #[must_use]
    pub const fn request_timeout_millis(&self) -> u64 {
        self.request_timeout_millis
    }

    #[must_use]
    pub const fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity
    }

    #[must_use]
    pub const fn max_restarts_per_window(&self) -> usize {
        self.max_restarts_per_window
    }

    #[must_use]
    pub const fn restart_window(&self) -> Duration {
        self.restart_window
    }
}

/// [`ProjectRuntimeConfig::from_env_with_file`] with the env lookup passed in.
fn parse_runtime_config(env: Env<'_>, file: &RuntimeFileConfig, max_workers: usize) -> Result<ProjectRuntimeConfig> {
    let defaults = ProjectRuntimeConfig::default_for_workers(max_workers);
    let positive = |name: &str, file, default| {
        config_file::positive_setting::<u64>(env, name, file, default).map_err(ServerError::Internal)
    };
    let positive_count = |name: &str, file, default| {
        config_file::positive_setting::<usize>(env, name, file, default).map_err(ServerError::Internal)
    };
    // Resolved before the heap ceiling, which defaults to a function of it:
    // an operator who raises the budget must get a ceiling that still sits
    // above it, or they have rebuilt the inversion `lean_max_memory_kib_for`
    // exists to prevent.
    // Configured in MiB because the value is gigabytes: a byte count here
    // would be a wall of zeros to get wrong.
    let import_residue_budget_bytes = positive(
        "LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB",
        file.worker_import_residue_budget_mib,
        defaults.import_residue_budget_bytes / (1024 * 1024),
    )?
    .saturating_mul(1024 * 1024);
    let config = ProjectRuntimeConfig {
        lean_max_memory_kib: positive(
            "LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB",
            file.lean_max_memory_kib,
            lean_max_memory_kib_for(import_residue_budget_bytes),
        )?,
        import_residue_budget_bytes,
        request_timeout_millis: positive(
            "LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS",
            file.request_timeout_millis,
            defaults.request_timeout_millis,
        )?,
        mailbox_capacity: positive_count(
            "LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY",
            file.project_mailbox_capacity,
            defaults.mailbox_capacity,
        )?,
        max_restarts_per_window: positive_count(
            "LEAN_HOST_MCP_WORKER_RESTART_LIMIT",
            file.worker_restart_limit,
            defaults.max_restarts_per_window,
        )?,
        restart_window: Duration::from_secs(positive(
            "LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS",
            file.worker_restart_window_secs,
            defaults.restart_window.as_secs(),
        )?),
        session_pool_capacity: positive_count(
            "LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY",
            file.worker_session_pool_capacity,
            defaults.session_pool_capacity,
        )?,
    };
    Ok(config)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "unit tests use expect/unwrap_err to state the branch under test directly"
)]
mod tests {
    use super::*;

    /// An env lookup that sets each `(name, value)` of `vars`.
    fn vars(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name| vars.iter().find(|(key, _)| *key == name).map(|(_, value)| value.into())
    }

    #[test]
    fn budget_sizing_estimate_is_debug_only_and_logged_once_at_startup() {
        let (_guard, buffer) = crate::request_log::tests::capture_at("lean_host_mcp", tracing::Level::DEBUG);
        let config = ProjectRuntimeConfig::default().with_import_residue_budget_bytes(4 * 1024 * 1024 * 1024);
        let broker = crate::broker::ProjectBroker::new_with_runtime_config(
            crate::broker::BrokerConfig {
                lake_root: None,
                max_workers: std::num::NonZeroUsize::MIN,
                max_workers_per_project: std::num::NonZeroUsize::MIN,
                max_lean_runs: std::num::NonZeroUsize::MIN,
                idle_timeout: Duration::ZERO,
            },
            config,
        );
        for _ in 0..3 {
            let _ = broker.runtime_status();
        }
        let output = buffer.text();
        assert_eq!(output.matches("two-import Mathlib sizing estimate").count(), 1);
        assert!(output.contains("this is supported, not a detected memory problem"));
        assert!(output.contains("budget_mib=4096 sizing_baseline_mib=9216"));
    }

    #[test]
    fn supported_small_budget_does_not_warn_but_invalid_heap_ceiling_does() {
        let (_guard, buffer) = crate::request_log::tests::capture_at("lean_host_mcp", tracing::Level::INFO);
        let mut config = ProjectRuntimeConfig::default().with_import_residue_budget_bytes(4 * 1024 * 1024 * 1024);
        config.log_advisories();
        assert!(buffer.text().is_empty());

        config.lean_max_memory_kib = 4 * 1024 * 1024;
        config.log_advisories();
        let output = buffer.text();
        assert!(output.contains("WARN"));
        assert!(output.contains("Lean heap ceiling is at or below the import residue budget"));
        assert!(!output.contains("two-import Mathlib"));
    }

    #[test]
    fn runtime_config_parses_runtime_policy_without_env_reads() {
        // Distinct values so a field routed to the wrong knob is visible.
        let config = parse_runtime_config(
            &vars(&[
                ("LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB", "5"),
                ("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS", "37"),
                ("LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY", "23"),
                ("LEAN_HOST_MCP_WORKER_RESTART_LIMIT", "29"),
                ("LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS", "31"),
                ("LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB", "41"),
                ("LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY", "43"),
            ]),
            &RuntimeFileConfig::default(),
            crate::broker::DEFAULT_MAX_WORKERS,
        )
        .unwrap();

        assert_eq!(config.lean_max_memory_kib(), 5);
        assert_eq!(config.request_timeout_millis(), 37);
        assert_eq!(config.mailbox_capacity(), 23);
        assert_eq!(config.max_restarts_per_window(), 29);
        assert_eq!(config.restart_window(), Duration::from_secs(31));
        assert_eq!(config.import_residue_budget_bytes(), 41 * 1024 * 1024);
        assert_eq!(config.session_pool_capacity(), 43);
    }

    #[test]
    fn the_residue_budget_defaults_to_one_childs_share_of_this_machine() {
        // Derived from system RAM and the pool size, so the assertion is on the
        // derivation and its bounds rather than a literal: the ceiling is
        // absolute, and the floor gives way to what one child's share of the
        // machine can actually hold.
        let ram = system_ram_bytes().unwrap_or(WORKER_ASSUMED_SYSTEM_RAM_BYTES);
        let budget = ProjectRuntimeConfig::default().import_residue_budget_bytes();
        assert_eq!(
            budget,
            import_residue_budget_for(ram, crate::broker::DEFAULT_MAX_WORKERS)
        );
        assert!(
            (WORKER_IMPORT_RESIDUE_MIN_BYTES..=WORKER_IMPORT_RESIDUE_CEILING_BYTES).contains(&budget),
            "derived residue budget {budget} escaped its bounds"
        );
        assert!(
            ProjectRuntimeConfig::default().import_residue_soft_bytes() < budget,
            "the idle threshold must fire before the reactive one, or it never fires at all"
        );
    }

    #[test]
    fn pool_capacity_and_the_residue_budget_are_independent_knobs() {
        // The bug this replaced: one constant serving as both the restart bound
        // and the pool capacity made the child's eviction path unreachable.
        let env = &vars(&[("LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY", "2")]);
        let config =
            parse_runtime_config(env, &RuntimeFileConfig::default(), crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.session_pool_capacity(), 2);
        assert_eq!(
            config.import_residue_budget_bytes(),
            ProjectRuntimeConfig::default().import_residue_budget_bytes(),
            "setting capacity must not move the memory bound"
        );
    }

    #[test]
    fn the_residue_budget_resolves_env_over_file_over_default() {
        let file = RuntimeFileConfig {
            worker_import_residue_budget_mib: Some(4_096),
            ..RuntimeFileConfig::default()
        };
        let config = parse_runtime_config(&|_| None, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.import_residue_budget_bytes(), 4_096 * 1024 * 1024);

        let env = &vars(&[("LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB", "2048")]);
        let config = parse_runtime_config(env, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.import_residue_budget_bytes(), 2_048 * 1024 * 1024);
    }

    #[test]
    fn request_timeout_precedence_env_over_file_over_default() {
        let file = RuntimeFileConfig {
            request_timeout_millis: Some(45_000),
            ..RuntimeFileConfig::default()
        };
        // Env unset -> file value is used.
        let config = parse_runtime_config(&|_| None, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.request_timeout_millis(), 45_000);
        // Env set -> env wins over the file.
        let env = &vars(&[("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS", "90000")]);
        let config = parse_runtime_config(env, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.request_timeout_millis(), 90_000);
        // Env empty -> unset, so the file value is used.
        let env = &vars(&[("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS", "")]);
        let config = parse_runtime_config(env, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.request_timeout_millis(), 45_000);
        // Neither -> built-in default (120 s).
        let config = parse_runtime_config(
            &|_| None,
            &RuntimeFileConfig::default(),
            crate::broker::DEFAULT_MAX_WORKERS,
        )
        .unwrap();
        assert_eq!(config.request_timeout_millis(), REQUEST_TIMEOUT_MILLIS);
    }

    #[test]
    fn request_timeout_zero_is_rejected() {
        // A zero deadline would time every call out instantly, so the resolver
        // must reject it.
        let err = parse_runtime_config(
            &vars(&[("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS", "0")]),
            &RuntimeFileConfig::default(),
            crate::broker::DEFAULT_MAX_WORKERS,
        )
        .unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(
            message.contains("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS"),
            "message: {message}"
        );
    }

    #[test]
    fn runtime_config_precedence_env_over_file_over_default() {
        let file = RuntimeFileConfig {
            lean_max_memory_kib: Some(8_388_608),
            ..RuntimeFileConfig::default()
        };
        // Env unset -> file value is used.
        let config = parse_runtime_config(&|_| None, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.lean_max_memory_kib(), 8_388_608);
        // Env set -> env wins over the file.
        let env = &vars(&[("LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB", "6291456")]);
        let config = parse_runtime_config(env, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
        assert_eq!(config.lean_max_memory_kib(), 6_291_456);
        // Neither -> built-in default.
        let config = parse_runtime_config(
            &|_| None,
            &RuntimeFileConfig::default(),
            crate::broker::DEFAULT_MAX_WORKERS,
        )
        .unwrap();
        assert_eq!(
            config.lean_max_memory_kib(),
            lean_max_memory_kib_for(config.import_residue_budget_bytes())
        );
    }

    /// The invariant the derivation exists for: crossing the heap ceiling aborts
    /// the child, so it must never sit at or below the point the supervisor
    /// recycles cleanly. A fixed 8 GiB ceiling under a 9 GiB budget floor made
    /// every Mathlib-scale recycle an abort and the residue policy unreachable —
    /// observed on a three-file kan-proofs sweep before this was derived.
    #[test]
    fn the_heap_ceiling_stays_above_every_residue_budget_the_resolver_can_produce() {
        // A raised budget must carry the ceiling with it, or the operator who
        // raised it has silently recreated the inversion.
        for budget_mib in [512_u64, 9216, 12288, 65536] {
            let file = RuntimeFileConfig {
                worker_import_residue_budget_mib: Some(budget_mib),
                ..RuntimeFileConfig::default()
            };
            let config = parse_runtime_config(&|_| None, &file, crate::broker::DEFAULT_MAX_WORKERS).unwrap();
            assert_eq!(config.import_residue_budget_bytes(), budget_mib * 1024 * 1024);
            assert!(
                config.lean_max_memory_kib().saturating_mul(1024) > config.import_residue_budget_bytes(),
                "ceiling {} KiB is not above budget {budget_mib} MiB",
                config.lean_max_memory_kib()
            );
        }
        // And the embedder-facing setter is not a way around it.
        let config = ProjectRuntimeConfig::default().with_import_residue_budget_bytes(40 * 1024 * 1024 * 1024);
        assert!(config.lean_max_memory_kib().saturating_mul(1024) > config.import_residue_budget_bytes());
    }

    /// "Above the budget" is not sufficient on its own. The heap counts several
    /// gigabytes the budget never sees — the elaborator's heap, the first
    /// import's non-residue allocation, the transient peak inside
    /// `importModules` — and that offset tracks the project, not the budget. A
    /// gap that is merely proportional vanishes at a low budget and puts the
    /// abort back in front of the recycle.
    #[test]
    fn the_gap_above_the_budget_never_falls_below_what_the_heap_counts_and_the_budget_does_not() {
        let gib = 1024 * 1024 * 1024;
        for budget in [1_u64, gib, 4 * gib, 9 * gib, 40 * gib] {
            let gap = lean_max_memory_kib_for(budget).saturating_mul(1024) - budget;
            // Minus one KiB: the ceiling is expressed in KiB, so a budget that
            // is not KiB-aligned loses its remainder to truncation.
            assert!(
                gap >= WORKER_HEAP_HEADROOM_FLOOR_BYTES - 1024,
                "budget {budget} leaves only {gap} bytes of heap headroom"
            );
        }
        // Above the floor the proportional term takes over, so a project heavy
        // enough to need a large budget gets a proportionally larger gap.
        assert_eq!(
            lean_max_memory_kib_for(40 * gib).saturating_mul(1024),
            80 * gib,
            "past the floor the gap must scale with the budget"
        );
    }

    /// The floor is unconditional and larger than several common machines. A
    /// budget the child cannot reach before the OS kills it is not a budget, so
    /// the machine gets the last word.
    #[test]
    fn a_machine_too_small_for_the_floor_gets_a_budget_it_can_actually_reach() {
        let gib = 1024 * 1024 * 1024;
        // Roomy: the floor decides, exactly as documented.
        assert_eq!(
            import_residue_budget_for(64 * gib, 4),
            WORKER_IMPORT_RESIDUE_FLOOR_BYTES
        );
        // Undersized: the floor would exceed the machine, so the machine wins
        // and `actor_main`'s below-floor warning becomes reachable.
        let small = import_residue_budget_for(8 * gib, 1);
        assert!(small < WORKER_IMPORT_RESIDUE_FLOOR_BYTES, "{small}");
        assert_eq!(small, 8 * gib - WORKER_IMPORT_HEADROOM_BYTES);
        // Undersized and crowded: four children on 8 GiB have 2 GiB each, less
        // than one import in flight, so each gets the degenerate minimum.
        assert_eq!(import_residue_budget_for(8 * gib, 4), WORKER_IMPORT_RESIDUE_MIN_BYTES);
        // Absurd: never zero, because zero fails the nonzero MiB round-trip and
        // would take the server down rather than degrade it.
        assert_eq!(import_residue_budget_for(gib, 4), WORKER_IMPORT_RESIDUE_MIN_BYTES);
        assert!(import_residue_budget_for(0, 4) > 0);
    }

    /// The budget is per child but the RAM is shared, so the pool size has to
    /// enter the derivation before the floor does. 24 GiB is the machine that
    /// crashed twice under four resident workers each sized as if alone.
    #[test]
    fn the_pool_size_divides_the_machine_before_the_floor_applies() {
        let gib = 1024 * 1024 * 1024;
        // One resident child: 24 − 4.5 GiB is left for it, so the floor is affordable.
        assert_eq!(
            import_residue_budget_for(24 * gib, 1),
            WORKER_IMPORT_RESIDUE_FLOOR_BYTES
        );
        // Two: 12 GiB each, 7.5 GiB of residue once the import in flight is paid for.
        assert_eq!(
            import_residue_budget_for(24 * gib, 2),
            12 * gib - WORKER_IMPORT_HEADROOM_BYTES
        );
        // Four: 6 GiB each, 1.5 GiB of residue. The old derivation handed each
        // the 9 GiB floor, 36 GiB of residue with 18 GiB heap ceilings on a
        // 24 GiB machine.
        assert_eq!(
            import_residue_budget_for(24 * gib, 4),
            6 * gib - WORKER_IMPORT_HEADROOM_BYTES
        );
        // The configured pool size, not the compiled-in default, sizes the defaults.
        let ram = system_ram_bytes().unwrap_or(WORKER_ASSUMED_SYSTEM_RAM_BYTES);
        assert_eq!(
            ProjectRuntimeConfig::default_for_workers(1).import_residue_budget_bytes(),
            import_residue_budget_for(ram, 1)
        );
        let one = parse_runtime_config(&|_| None, &RuntimeFileConfig::default(), 1).unwrap();
        let four = parse_runtime_config(&|_| None, &RuntimeFileConfig::default(), 4).unwrap();
        assert!(one.import_residue_budget_bytes() >= four.import_residue_budget_bytes());
        assert!(one.lean_max_memory_kib() >= four.lean_max_memory_kib());
    }

    #[test]
    fn runtime_config_rejects_zero_from_file() {
        // Zero is rejected wherever it comes from: an unbounded Lean heap is a
        // different posture from a large one, and reaching it by typing `0`
        // into a config file is never deliberate.
        let zero = RuntimeFileConfig {
            lean_max_memory_kib: Some(0),
            ..RuntimeFileConfig::default()
        };
        let err = parse_runtime_config(&|_| None, &zero, crate::broker::DEFAULT_MAX_WORKERS).unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(
            message.contains("LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB"),
            "message: {message}"
        );
    }
}
