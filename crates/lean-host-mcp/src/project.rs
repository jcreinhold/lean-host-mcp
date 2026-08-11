//! `LeanProject`—the unit of Lean semantic execution.
//!
//! One Lake project owns one private serialized controller. The controller
//! submits one worker request at a time, applies host memory/retry policy, and
//! exposes only typed request/reply calls to tool modules. The lower
//! `lean-rs-worker-parent` service owns child-process shutdown, generation
//! separation, terminal outcomes, and primitive restart mechanics.

#![allow(let_underscore_drop, clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerCapabilityBuilder, LeanWorkerChild, LeanWorkerDeclarationInspectionRequest,
    LeanWorkerDeclarationInspectionResult, LeanWorkerDeclarationSearch, LeanWorkerDeclarationSearchResult,
    LeanWorkerDeclarationVerificationBatchRequest, LeanWorkerDeclarationVerificationBatchResult,
    LeanWorkerDeclarationVerificationRequest, LeanWorkerDeclarationVerificationResult, LeanWorkerElabOptions,
    LeanWorkerError, LeanWorkerHostHandle, LeanWorkerHostHandleBuilder, LeanWorkerLifecycleSnapshot,
    LeanWorkerModuleCacheLimits, LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchOutcome,
    LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets,
    LeanWorkerProofAttemptRequest, LeanWorkerProofAttemptResult, LeanWorkerRestartPolicy, LeanWorkerRestartReason,
};
use lean_semantic_search_runtime::SemanticSearchRuntimeBuild;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::cache::ModuleQueryCache;
use crate::config_file::RuntimeFileConfig;
use crate::envelope::{Freshness, RuntimeFacts, RuntimeRestartEvent};
use crate::error::{Result, ServerError, WorkerUnavailable, map_worker_err};
use crate::lake_meta::{self, LakeProjectMeta};
use crate::semantic_search::{SemanticProofSearchRequest, SemanticProofSearchResult};
use crate::toolchain::{Readiness, ToolchainId, WorkerBinary};

/// LRU capacity for exact bounded module-query results, applied to the single
/// and batch caches separately.
///
/// This bounds memory outright rather than approximately. Every cached value is
/// a worker reply already truncated to the 64 KiB `total_bytes` budget the tools
/// send (`DEFAULT_TOTAL_BYTES` in `tools/`), so one project holds at most
/// `2 × 64 × 64 KiB = 8 MiB` and the default four-project pool at most 32 MiB —
/// no byte-accounting LRU required, which is the machinery this design avoids.
///
/// 64 rather than the former 256 because the working set is small and concrete:
/// an agent iterates on a handful of files, and each file contributes one entry
/// per distinct query shape. 256 sized the cache for a workload nobody has;
/// what it actually bought was a 4× larger worst case.
const MODULE_QUERY_CACHE_CAPACITY: usize = 64;
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
/// Reciprocal of the share of system RAM all resident projects' residue budgets
/// may claim, before division by `BrokerConfig::max_projects`.
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
/// Pure backstop on imports per child, behind the byte budget.
///
/// The one thing between a residue-accounting bug and the 11.2 GiB `SIGKILL`
/// this subsystem exists to prevent, and it costs one integer compare. Set far
/// above any legitimate generation so that in normal operation the byte bound is
/// always what fires and this only speaks when residue is being reported as
/// zero. [`LeanWorkerStats::import_stats_unusable`] says which happened.
const WORKER_MAX_IMPORTS_BACKSTOP: u64 = 32;
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
/// `8` because [`crate::tools::changed_coverage`] loops per changed file with no
/// cap and median PR diffs run 3–12 files; at 4, any 5-file diff thrashes the
/// pool inside one tool call. Eight costs ~560 MiB held against 8 GB or more of
/// re-imports avoided.
const WORKER_SESSION_POOL_CAPACITY: usize = 8;
/// How long the actor waits for another call before treating the project as
/// quiescent and cycling an over-budget child.
///
/// Only armed when the child is already over its soft residue budget; under it
/// the actor blocks on its mailbox with no timer at all. Two seconds is short
/// enough that a pause between proof steps is usually enough, and long enough
/// that a burst of calls is never interrupted mid-flight.
const QUIESCENCE_GRACE: Duration = Duration::from_secs(2);
/// How recently a profile must have been served for an idle cycle to re-import
/// it into the fresh child.
///
/// A minute of silence is not evidence the caller is coming back to the same
/// file, and a wrong guess costs a full import.
const PREWARM_RECENCY: Duration = Duration::from_mins(1);
const MAX_JOB_RETRIES: u32 = 1;
const MAX_RESTARTS_PER_WINDOW: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_mins(1);
/// Backstop on *every* worker cycle in one [`RESTART_WINDOW`], planned or not.
///
/// The supervisor's own limit, distinct from [`MAX_RESTARTS_PER_WINDOW`], which
/// this actor applies to abnormal causes only. The supervisor cannot make that
/// distinction, so its default of 16 counts [`WORKER_MAX_IMPORTS`] cycles —
/// and exhausting it is terminal: the supervisor refuses to spawn a
/// replacement and every later call fails with "shutdown is in progress". A
/// workload that alternates import profiles reached that state after 16 calls
/// and never recovered. Sized far above any legitimate cycle rate so that only
/// a genuine spawn loop can reach it; the discriminating breaker is this
/// actor's own.
const SUPERVISOR_RESTART_INTENSITY: u64 = 256;

/// Runtime policy for one private project actor.
///
/// The binary parses this once at server startup and passes it into the
/// broker. Tests and embedders can construct the default directly without
/// rereading process environment during project open.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectRuntimeConfig {
    lean_max_memory_kib: u64,
    request_timeout_millis: u64,
    mailbox_capacity: usize,
    max_restarts_per_window: usize,
    restart_window: Duration,
    import_residue_budget_bytes: u64,
    session_pool_capacity: usize,
}

impl Default for ProjectRuntimeConfig {
    fn default() -> Self {
        let import_residue_budget_bytes = default_import_residue_budget_bytes(crate::broker::DEFAULT_MAX_PROJECTS);
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
}

/// The residue budget one project actor gets when nothing overrides it.
///
/// Derived rather than constant because the quantity it bounds is a share of the
/// machine: each resident project owns one child, up to `max_projects` of them
/// are resident at once, and residue is the part of a child that cannot be
/// handed back. The floor is what actually decides the value on any machine
/// smaller than about 150 GiB, which is deliberate — see
/// [`WORKER_IMPORT_RESIDUE_FLOOR_BYTES`]. Sizing below one import's residue
/// would be strictly worse than the count bound this replaces, so the fraction
/// is allowed to lose.
fn default_import_residue_budget_bytes(max_projects: usize) -> u64 {
    import_residue_budget_for(
        system_ram_bytes().unwrap_or(WORKER_ASSUMED_SYSTEM_RAM_BYTES),
        max_projects,
    )
}

/// The policy half of [`default_import_residue_budget_bytes`], split out from
/// the one platform read so the sizing rules can be tested on machines that do
/// not exist.
fn import_residue_budget_for(ram: u64, max_projects: usize) -> u64 {
    let per_project = ram
        .checked_div(WORKER_IMPORT_RESIDUE_RAM_DIVISOR)
        .and_then(|share| share.checked_div(max_projects.max(1) as u64))
        .unwrap_or(WORKER_IMPORT_RESIDUE_FLOOR_BYTES);
    // The floor is unconditional, and on any machine under ~12 GiB it exceeds
    // the whole machine — a budget the child cannot reach before the OS kills it
    // is not a budget, and the `SIGKILL` it fails to prevent is the exact
    // failure this subsystem exists for. Cap it at what is left after the one
    // import that may be in flight when the bound is tested. On an
    // undersized machine the result lands below the floor, which is honest:
    // that is what `actor_main`'s below-floor warning is there to say.
    let affordable = ram
        .saturating_sub(WORKER_IMPORT_HEADROOM_BYTES)
        .max(WORKER_IMPORT_RESIDUE_MIN_BYTES);
    per_project
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
        Self::from_env_with_file(&RuntimeFileConfig::default())
    }

    /// Resolve the runtime policy with a config-file section as the layer
    /// beneath env vars: each knob is `env var > file > built-in default`.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when an env var is malformed, or a resolved
    /// value (from env or file) is zero where zero is unsafe.
    pub fn from_env_with_file(file: &RuntimeFileConfig) -> Result<Self> {
        parse_runtime_config(
            RuntimeEnv {
                lean_max_memory_kib: runtime_env_var("LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB")?,
                request_timeout_millis: runtime_env_var("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS")?,
                project_mailbox_capacity: runtime_env_var("LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY")?,
                worker_restart_limit: runtime_env_var("LEAN_HOST_MCP_WORKER_RESTART_LIMIT")?,
                worker_restart_window_secs: runtime_env_var("LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS")?,
                worker_import_residue_budget_mib: runtime_env_var("LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB")?,
                worker_session_pool_capacity: runtime_env_var("LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY")?,
            },
            file,
        )
    }

    /// Unreclaimable import residue one worker child may retain before the
    /// supervisor cycles it; see [`WORKER_IMPORT_RESIDUE_CEILING_BYTES`].
    #[must_use]
    pub const fn import_residue_budget_bytes(&self) -> u64 {
        self.import_residue_budget_bytes
    }

    /// Override the residue budget on an already-resolved config.
    ///
    /// The one knob an embedder that bypasses [`Self::from_env`] still has to
    /// be able to set: every other default is a policy constant that holds on
    /// any machine, while this one is a share of the host's RAM and the default
    /// can only guess at how many projects will actually be resident. Also how
    /// a test forces the residue path to fire without a Mathlib-scale import.
    ///
    /// Moves the Lean heap ceiling with it, for the reason
    /// [`lean_max_memory_kib_for`] gives: a ceiling below the budget aborts the
    /// child where the budget would have recycled it cleanly. Callers do not get
    /// to hold the two independently, because there is no correct way to.
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

    /// How many imported environments one worker child pools; see
    /// [`WORKER_SESSION_POOL_CAPACITY`].
    #[must_use]
    pub const fn session_pool_capacity(&self) -> usize {
        self.session_pool_capacity
    }

    /// The Lean heap ceiling applied to each worker child; see
    /// [`lean_max_memory_kib_for`].
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

#[derive(Debug, Default)]
struct RuntimeEnv {
    lean_max_memory_kib: Option<String>,
    request_timeout_millis: Option<String>,
    project_mailbox_capacity: Option<String>,
    worker_restart_limit: Option<String>,
    worker_restart_window_secs: Option<String>,
    worker_import_residue_budget_mib: Option<String>,
    worker_session_pool_capacity: Option<String>,
}

fn parse_runtime_config(env: RuntimeEnv, file: &RuntimeFileConfig) -> Result<ProjectRuntimeConfig> {
    let defaults = ProjectRuntimeConfig::default();
    // Resolved before the heap ceiling, which defaults to a function of it:
    // an operator who raises the budget must get a ceiling that still sits
    // above it, or they have rebuilt the inversion `lean_max_memory_kib_for`
    // exists to prevent.
    // Configured in MiB because the value is gigabytes: a byte count here
    // would be a wall of zeros to get wrong.
    let import_residue_budget_bytes = parse_nonzero_u64(
        "LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB",
        env.worker_import_residue_budget_mib.as_deref(),
        file.worker_import_residue_budget_mib,
        defaults.import_residue_budget_bytes / (1024 * 1024),
    )?
    .saturating_mul(1024 * 1024);
    let config = ProjectRuntimeConfig {
        lean_max_memory_kib: parse_nonzero_u64(
            "LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB",
            env.lean_max_memory_kib.as_deref(),
            file.lean_max_memory_kib,
            lean_max_memory_kib_for(import_residue_budget_bytes),
        )?,
        import_residue_budget_bytes,
        request_timeout_millis: parse_nonzero_u64(
            "LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS",
            env.request_timeout_millis.as_deref(),
            file.request_timeout_millis,
            defaults.request_timeout_millis,
        )?,
        mailbox_capacity: parse_nonzero_usize(
            "LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY",
            env.project_mailbox_capacity.as_deref(),
            file.project_mailbox_capacity,
            defaults.mailbox_capacity,
        )?,
        max_restarts_per_window: parse_nonzero_usize(
            "LEAN_HOST_MCP_WORKER_RESTART_LIMIT",
            env.worker_restart_limit.as_deref(),
            file.worker_restart_limit,
            defaults.max_restarts_per_window,
        )?,
        restart_window: Duration::from_secs(parse_nonzero_u64(
            "LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS",
            env.worker_restart_window_secs.as_deref(),
            file.worker_restart_window_secs,
            defaults.restart_window.as_secs(),
        )?),
        session_pool_capacity: parse_nonzero_usize(
            "LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY",
            env.worker_session_pool_capacity.as_deref(),
            file.worker_session_pool_capacity,
            defaults.session_pool_capacity,
        )?,
    };
    Ok(config)
}

fn runtime_env_var(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err @ std::env::VarError::NotUnicode(_)) => {
            Err(ServerError::Internal(format!("{name} is not valid unicode: {err}")))
        }
    }
}

/// Result of one project actor call.
#[derive(Debug, Clone)]
pub(crate) struct ProjectCall<T> {
    value: T,
    runtime: RuntimeFacts,
}

impl<T> ProjectCall<T> {
    pub(crate) fn new(value: T, runtime: RuntimeFacts) -> Self {
        Self { value, runtime }
    }

    pub(crate) fn into_parts(self) -> (T, RuntimeFacts) {
        (self.value, self.runtime)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetryPolicy {
    RetryOnceReadOnly,
}

impl RetryPolicy {
    fn retries(self) -> u32 {
        match self {
            Self::RetryOnceReadOnly => MAX_JOB_RETRIES,
        }
    }
}

struct ActiveJobGuard {
    active_jobs: Arc<AtomicUsize>,
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::AcqRel);
    }
}

struct JobMeta {
    imports: Vec<String>,
    import_fingerprint: String,
    _created_at: Instant,
    queued_at: Instant,
    _correlation_id: uuid::Uuid,
    retry_policy: RetryPolicy,
    _active_job: ActiveJobGuard,
}

enum ProjectMessage {
    ModuleQuery {
        meta: JobMeta,
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerModuleQueryOutcome>>>,
    },
    ModuleQueryBatch {
        meta: JobMeta,
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerModuleQueryBatchOutcome>>>,
    },
    DeclarationInspection {
        meta: JobMeta,
        request: LeanWorkerDeclarationInspectionRequest,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerDeclarationInspectionResult>>>,
    },
    /// Several searches against **one** session; see
    /// [`LeanProject::search_declarations`].
    DeclarationSearch {
        meta: JobMeta,
        requests: Vec<LeanWorkerDeclarationSearch>,
        reply: oneshot::Sender<Result<ProjectCall<Vec<LeanWorkerDeclarationSearchResult>>>>,
    },
    ProofAttempt {
        meta: JobMeta,
        request: LeanWorkerProofAttemptRequest,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerProofAttemptResult>>>,
    },
    DeclarationVerification {
        meta: JobMeta,
        request: LeanWorkerDeclarationVerificationRequest,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerDeclarationVerificationResult>>>,
    },
    DeclarationVerificationBatch {
        meta: JobMeta,
        request: LeanWorkerDeclarationVerificationBatchRequest,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerDeclarationVerificationBatchResult>>>,
    },
    SemanticProofSearch {
        meta: JobMeta,
        request: SemanticProofSearchRequest,
        reply: oneshot::Sender<Result<ProjectCall<SemanticProofSearchResult>>>,
    },
}

impl ProjectMessage {
    fn imports(&self) -> &[String] {
        match self {
            Self::ModuleQuery { meta, .. }
            | Self::ModuleQueryBatch { meta, .. }
            | Self::DeclarationInspection { meta, .. }
            | Self::DeclarationSearch { meta, .. }
            | Self::ProofAttempt { meta, .. }
            | Self::DeclarationVerification { meta, .. }
            | Self::DeclarationVerificationBatch { meta, .. }
            | Self::SemanticProofSearch { meta, .. } => &meta.imports,
        }
    }

    fn reject(self, state: &ProjectActorState, reason: &'static str) {
        match self {
            Self::ModuleQuery { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::ModuleQueryBatch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationInspection { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationSearch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::ProofAttempt { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationVerification { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationVerificationBatch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::SemanticProofSearch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestartCause {
    /// A `.olean` among the live session's imports was rebuilt on disk. The
    /// session's environment is a snapshot taken at import, so it must be
    /// dropped or the call answers from a stale Lean environment.
    ArtifactsRebuilt,
    RssPostJob,
    RssHardLimit,
    MaxRequests,
    MaxImports,
    /// Imports retained the configured budget of unreclaimable bytes, and the
    /// child was cycled before the next one on the request path.
    ImportResidue,
    /// The same budget crossed its soft threshold while the mailbox was empty,
    /// so the cycle ran between requests instead of inside one. The ratio of
    /// this cause to [`Self::ImportResidue`] is how much of the recycle cost the
    /// idle path actually moved off the critical path.
    ImportResidueIdle,
    Idle,
    Timeout,
    Cancelled,
    ChildExit,
    ChildAbort,
    SessionMissing,
    Explicit,
    WorkerInternal,
}

impl RestartCause {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ArtifactsRebuilt => "artifacts_rebuilt",
            Self::RssPostJob => "rss_post_job",
            Self::RssHardLimit => "rss_hard_limit_exceeded",
            Self::MaxRequests => "max_requests",
            Self::MaxImports => "max_imports",
            Self::ImportResidue => "import_residue",
            Self::ImportResidueIdle => "import_residue_idle",
            Self::Idle => "idle",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ChildExit => "child_exit",
            Self::ChildAbort => "child_abort",
            Self::SessionMissing => "session_missing",
            Self::Explicit => "explicit",
            Self::WorkerInternal => "worker_internal",
        }
    }

    const fn counts_toward_restart_limit(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Cancelled
                | Self::ChildExit
                | Self::ChildAbort
                | Self::SessionMissing
                | Self::RssHardLimit
                // Not a planned cycle: this is the bucket for a restart whose
                // cause this build cannot name — an upstream policy added after
                // it shipped, or a replacement the supervisor reported with no
                // reason at all. `diagnosis::execution_taint` already treats it
                // as disrupting; a crash-loop breaker that excused the one cause
                // it understands least would be exactly backwards.
                | Self::WorkerInternal
        )
    }

    const fn is_planned(self) -> bool {
        !self.counts_toward_restart_limit()
    }
}

fn restart_event(
    cause: RestartCause,
    reason: impl Into<String>,
    worker_generation: u64,
    rss_kib: Option<u64>,
    limit_kib: Option<u64>,
) -> RuntimeRestartEvent {
    RuntimeRestartEvent {
        cause: cause.as_str().to_owned(),
        reason: reason.into(),
        worker_generation,
        planned: cause.is_planned(),
        rss_kib,
        limit_kib,
    }
}

/// Per-project recycle tally over the worker's lifetime, all causes.
///
/// Recorded once per event at [`ProjectActorState::record_restart`] and copied
/// into [`RuntimeSnapshot`] on publish, so the no-call and error paths report
/// the same totals a live call would. This answers "how *often*, and why?"; the
/// single most-recent event stays in `last_restart`.
#[derive(Debug, Clone, Default)]
struct RestartStats {
    total: u64,
    by_cause: BTreeMap<String, u64>,
}

impl RestartStats {
    fn observe(&mut self, cause: &str) {
        self.total = self.total.saturating_add(1);
        let count = self.by_cause.entry(cause.to_owned()).or_default();
        *count = count.saturating_add(1);
    }
}

/// Emit one structured log line for a recycle. Level tracks the *signal*, not
/// `planned`: crash/abnormal causes `warn`, memory-pressure cycles `info` (the
/// frequency an operator tuning the RSS budget watches), pure hygiene `debug`.
fn log_restart(event: &RuntimeRestartEvent, restarts_total: u64) {
    macro_rules! emit {
        ($level:ident, $msg:literal) => {
            tracing::$level!(
                cause = %event.cause,
                reason = %event.reason,
                worker_generation = event.worker_generation,
                rss_kib = ?event.rss_kib,
                limit_kib = ?event.limit_kib,
                planned = event.planned,
                restarts_total,
                $msg
            )
        };
    }
    match event.cause.as_str() {
        "rss_hard_limit_exceeded"
        | "child_abort"
        | "child_exit"
        | "session_missing"
        | "worker_internal"
        | "timeout"
        | "cancelled" => emit!(warn, "worker recycled (abnormal)"),
        "rss_post_job" => emit!(info, "worker recycled (memory pressure)"),
        "artifacts_rebuilt" => emit!(info, "worker recycled (imports rebuilt on disk)"),
        // Reactive at `info`, proactive at `debug` (via the fallthrough): the
        // ratio between them is the tuning signal, and it is the *reactive*
        // half — the cycle a call had to wait for — that an operator acts on.
        "import_residue" => emit!(info, "worker recycled (import residue budget)"),
        _ => emit!(debug, "worker recycled (hygiene)"),
    }
}

fn restart_cause_from_worker(reason: &LeanWorkerRestartReason) -> RestartCause {
    match reason.stable_cause() {
        "explicit" => RestartCause::Explicit,
        "max_requests" => RestartCause::MaxRequests,
        "max_imports" => RestartCause::MaxImports,
        "import_residue" => RestartCause::ImportResidue,
        "rss_ceiling" => RestartCause::RssPostJob,
        "rss_hard_limit" => RestartCause::RssHardLimit,
        "idle" => RestartCause::Idle,
        "cancelled" => RestartCause::Cancelled,
        "timeout" => RestartCause::Timeout,
        "child_abort" => RestartCause::ChildAbort,
        _ => RestartCause::WorkerInternal,
    }
}

#[derive(Debug, Clone)]
struct RuntimeSnapshot {
    worker_generation: u64,
    last_restart: Option<RuntimeRestartEvent>,
    rss_kib: Option<u64>,
    import_profile: Option<String>,
    profile_switch_count: u64,
    restarts_total: u64,
    restarts_by_cause: BTreeMap<String, u64>,
    import_residue_bytes: Option<u64>,
    import_residue_limit_bytes: Option<u64>,
}

impl RuntimeSnapshot {
    fn facts(&self) -> RuntimeFacts {
        RuntimeFacts {
            worker_generation: self.worker_generation,
            worker_restarted: false,
            retry_count: 0,
            queue_wait_millis: 0,
            call_restart: None,
            last_restart: self.last_restart.clone(),
            rss_kib: self.rss_kib,
            worker_lanes: 1,
            import_profile: self.import_profile.clone(),
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restarts_total,
            restarts_by_cause: self.restarts_by_cause.clone(),
            import_residue_bytes: self.import_residue_bytes,
            import_residue_limit_bytes: self.import_residue_limit_bytes,
        }
    }
}

/// One Lake project, one serialized worker controller, one in-memory cache.
/// Cheap to clone via `Arc`.
pub(crate) struct LeanProject {
    canonical_root: PathBuf,
    toolchain: String,
    package: Option<String>,
    library: Option<String>,
    manifest_hash: String,
    session_id: String,
    /// Toolchain-provenance advisories captured at open (unknown pin, missing
    /// sidecar). Surfaced into every response's envelope warnings via
    /// [`Self::freshness`]; empty for a fully-vouched-for worker.
    open_warnings: Vec<String>,
    actor_tx: Mutex<Option<mpsc::Sender<ProjectMessage>>>,
    active_jobs: Arc<AtomicUsize>,
    healthy: Arc<AtomicBool>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    module_queries: ModuleQueryCache,
}

impl std::fmt::Debug for LeanProject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeanProject")
            .field("canonical_root", &self.canonical_root)
            .field("toolchain", &self.toolchain)
            .field("package", &self.package)
            .field("library", &self.library)
            .field("manifest_hash", &self.manifest_hash)
            .finish_non_exhaustive()
    }
}

impl LeanProject {
    pub(crate) fn open(meta: LakeProjectMeta, runtime_config: ProjectRuntimeConfig) -> Result<Arc<Self>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let runtime = Arc::new(Mutex::new(RuntimeSnapshot {
            worker_generation: 1,
            last_restart: None,
            rss_kib: None,
            import_profile: None,
            profile_switch_count: 0,
            restarts_total: 0,
            restarts_by_cause: BTreeMap::new(),
            import_residue_bytes: None,
            import_residue_limit_bytes: None,
        }));
        let active_jobs = Arc::new(AtomicUsize::new(0));
        let healthy = Arc::new(AtomicBool::new(true));
        let (config, open_warnings) = ActorConfig::from_meta(
            &meta,
            session_id.clone(),
            Arc::clone(&runtime),
            Arc::clone(&healthy),
            runtime_config,
        )?;
        type InitMsg = std::result::Result<(String, mpsc::Sender<ProjectMessage>), ServerError>;
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();
        let thread_name = actor_thread_name(&meta.canonical_root);

        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                actor_main(config, init_tx);
            })
            .map_err(|e| ServerError::Internal(format!("spawn project actor thread: {e}")))?;

        let (runtime_toolchain, actor_tx) = init_rx
            .recv()
            .map_err(|_| ServerError::Internal("project actor thread died during init".into()))??;

        let cache_cap = NonZeroUsize::new(MODULE_QUERY_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN);
        Ok(Arc::new(Self {
            canonical_root: meta.canonical_root,
            toolchain: runtime_toolchain,
            package: meta.package,
            library: meta.library,
            manifest_hash: meta.manifest_hash,
            session_id,
            open_warnings,
            actor_tx: Mutex::new(Some(actor_tx)),
            active_jobs,
            healthy,
            runtime,
            module_queries: ModuleQueryCache::with_capacity(cache_cap),
        }))
    }

    /// Process one module query through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn process_module_query(
        &self,
        imports: Vec<String>,
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQuery {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            source,
            query,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Process one module-query batch through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn process_module_query_batch(
        &self,
        imports: Vec<String>,
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryBatchOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQueryBatch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            source,
            selectors,
            budgets,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Inspect one declaration through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn inspect_declaration(
        &self,
        imports: Vec<String>,
        request: LeanWorkerDeclarationInspectionRequest,
    ) -> Result<ProjectCall<LeanWorkerDeclarationInspectionResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationInspection {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            request,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Run a group of bounded declaration searches through this project's
    /// serialized worker actor, against one session.
    ///
    /// The unit is a group rather than a single search because its one caller
    /// — `search_for_proof` — always has several: it derives up to six queries
    /// from a single goal and needs all of them to rank. Issued one at a time
    /// those were N mailbox hops and, worse, N session opens, and every open
    /// re-runs `importModules` over the whole closure. Results come back in
    /// request order.
    ///
    /// Worth the widened signature: `benches/search_for_proof.rs` on the
    /// fixture went from **2.71 s to 1.41 s** per warm call — the six session
    /// opens were about half of the most expensive tool in the surface, and the
    /// saving scales with the import closure, so a Mathlib-scale project gains
    /// more than this fixture does.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or worker
    /// execution fails. A failure of any single search fails the group: they
    /// share a session, so a worker fault that kills one has already invalidated
    /// the rest.
    pub(crate) async fn search_declarations(
        &self,
        imports: Vec<String>,
        requests: Vec<LeanWorkerDeclarationSearch>,
    ) -> Result<ProjectCall<Vec<LeanWorkerDeclarationSearchResult>>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationSearch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            requests,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Run source-backed semantic proof search through this project's actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when semantic capability setup, mailbox enqueue,
    /// actor reply, or worker execution fails.
    pub(crate) async fn semantic_proof_search(
        &self,
        imports: Vec<String>,
        request: SemanticProofSearchRequest,
    ) -> Result<ProjectCall<SemanticProofSearchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::SemanticProofSearch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            request,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Try proof fragments in-memory through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn attempt_proof(
        &self,
        imports: Vec<String>,
        request: LeanWorkerProofAttemptRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerProofAttemptResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ProofAttempt {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Verify one declaration in-memory through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn verify_declaration(
        &self,
        imports: Vec<String>,
        request: LeanWorkerDeclarationVerificationRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerification {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Verify several declarations in one in-memory source snapshot through
    /// this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn verify_declaration_batch(
        &self,
        imports: Vec<String>,
        request: LeanWorkerDeclarationVerificationBatchRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationBatchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerificationBatch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    fn job_meta(&self, imports: Vec<String>, retry_policy: RetryPolicy) -> JobMeta {
        let created_at = Instant::now();
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        JobMeta {
            import_fingerprint: import_fingerprint(&imports),
            imports,
            _created_at: created_at,
            queued_at: Instant::now(),
            _correlation_id: uuid::Uuid::new_v4(),
            retry_policy,
            _active_job: ActiveJobGuard {
                active_jobs: Arc::clone(&self.active_jobs),
            },
        }
    }

    async fn enqueue<T>(
        &self,
        message: ProjectMessage,
        reply_rx: oneshot::Receiver<Result<ProjectCall<T>>>,
    ) -> Result<ProjectCall<T>>
    where
        T: Send + 'static,
    {
        let project_info = self.worker_error_context(message.imports());
        let tx = self
            .actor_tx
            .lock()
            .as_ref()
            .cloned()
            .ok_or_else(|| self.unavailable("project actor is stopped", false, false))?;
        match tx.try_send(message) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(ServerError::worker_unavailable(WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    reason: "mailbox_full".to_owned(),
                    ..project_info
                }));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.shutdown();
                return Err(ServerError::worker_unavailable(WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    reason: "mailbox_closed".to_owned(),
                    ..project_info
                }));
            }
        }

        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.shutdown();
                Err(self.unavailable("mailbox_closed_before_reply", true, false))
            }
        }
    }

    pub(crate) fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub(crate) fn toolchain(&self) -> &str {
        &self.toolchain
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub(crate) fn module_query_cache(&self) -> &ModuleQueryCache {
        &self.module_queries
    }

    #[must_use]
    pub(crate) fn freshness(&self, request_imports: &[String]) -> Freshness {
        Freshness {
            project_root: self.canonical_root.to_string_lossy().into_owned(),
            project_hash: self.manifest_hash.clone(),
            imports: request_imports.to_vec(),
            session_id: self.session_id.clone(),
            lean_toolchain: self.toolchain.clone(),
            toolchain_advisories: self.open_warnings.clone(),
        }
    }

    #[must_use]
    pub(crate) fn runtime_facts(&self) -> RuntimeFacts {
        self.runtime.lock().facts()
    }

    pub(crate) fn shutdown(&self) {
        self.healthy.store(false, Ordering::Release);
        let _ = self.actor_tx.lock().take();
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && self.actor_tx.lock().as_ref().is_some_and(|tx| !tx.is_closed())
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.active_jobs.load(Ordering::Acquire) == 0
    }

    fn unavailable(&self, reason: impl Into<String>, retryable: bool, worker_restarted: bool) -> ServerError {
        ServerError::worker_unavailable(WorkerUnavailable {
            retryable,
            worker_restarted,
            reason: reason.into(),
            ..self.worker_error_context(&[])
        })
    }

    fn worker_error_context(&self, imports: &[String]) -> WorkerUnavailable {
        let snapshot = self.runtime.lock().clone();
        let runtime = snapshot.facts();
        WorkerUnavailable {
            retryable: true,
            worker_restarted: false,
            project_root: self.canonical_root.to_string_lossy().into_owned(),
            project_hash: self.manifest_hash.clone(),
            imports: imports.to_vec(),
            session_id: self.session_id.clone(),
            lean_toolchain: self.toolchain.clone(),
            worker_generation: snapshot.worker_generation,
            reason: String::new(),
            restart_cause: snapshot.last_restart.as_ref().map(|event| event.cause.clone()),
            rss_kib: snapshot.rss_kib,
            limit_kib: None,
            retry_after_millis: None,
            restarts_in_window: None,
            window_millis: None,
            runtime,
            toolchain_advisories: self.open_warnings.clone(),
        }
    }
}

impl Drop for LeanProject {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
struct ActorConfig {
    lake_root: PathBuf,
    manifest_hash: String,
    toolchain_label: String,
    worker_path: PathBuf,
    lean_sysroot: PathBuf,
    session_id: String,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    healthy: Arc<AtomicBool>,
    /// Where a module this project imports could have its `.olean` built.
    /// Resolved once at open: the package set is a function of
    /// `lake-manifest.json`, and a manifest change evicts the project outright.
    artifact_roots: Vec<PathBuf>,
    lean_max_memory_kib: u64,
    request_timeout_millis: u64,
    mailbox_capacity: usize,
    max_restarts_per_window: usize,
    restart_window: Duration,
    import_residue_budget_bytes: u64,
    import_residue_soft_bytes: u64,
    session_pool_capacity: usize,
    /// The tokio runtime the actor may borrow to wait on its mailbox with a
    /// timeout, captured where [`LeanProject::open`] runs inside one.
    ///
    /// `None` in a synchronous test context, which degrades to the mailbox-empty
    /// check alone — the same degradation the broker's reaper already accepts.
    /// The handle is used *only* to wrap the receive; no Lean session ever
    /// crosses it.
    tokio_handle: Option<tokio::runtime::Handle>,
    /// Open-time toolchain advisories (unknown pin, missing sidecar, no smoke
    /// record). The actor carries them so a `runtime_unavailable` it produces
    /// after worker death still flags a suspect worker. Mirrors
    /// [`LeanProject::open_warnings`]; both come from the one
    /// [`WorkerBinary::resolve_ready_for`] verdict at open.
    toolchain_advisories: Vec<String>,
}

impl ActorConfig {
    /// Resolve the pinned toolchain into a spawnable config plus any
    /// open-time provenance advisories. All version-drift situations collapse
    /// into the one [`WorkerBinary::resolve_ready_for`] verdict: hard failures
    /// become a typed [`ServerError::BadProject`] carrying the corrective
    /// command; soft ones (unknown pin, missing sidecar) ride along as
    /// warnings the project surfaces in every envelope.
    fn from_meta(
        meta: &LakeProjectMeta,
        session_id: String,
        runtime: Arc<Mutex<RuntimeSnapshot>>,
        healthy: Arc<AtomicBool>,
        runtime_config: ProjectRuntimeConfig,
    ) -> Result<(Self, Vec<String>)> {
        let toolchain_id = ToolchainId::parse(&meta.toolchain).map_err(|e| ServerError::BadProject(e.to_string()))?;
        let (worker_path, lean_sysroot, open_warnings) = match WorkerBinary::resolve_ready_for(&toolchain_id) {
            Readiness::Ready {
                worker,
                lean_sysroot,
                note,
            } => (worker.path, lean_sysroot, note.into_iter().collect()),
            Readiness::UnknownPin {
                pin,
                worker,
                lean_sysroot,
            } => (
                worker.path,
                lean_sysroot,
                vec![format!(
                    "lean-toolchain pins {pin}, which is not a recognized lean-rs supported version \
                     (e.g. a nightly); proceeding, but the host cannot vouch for ABI compatibility"
                )],
            ),
            Readiness::Unsupported { window, nearest } => {
                return Err(ServerError::BadProject(format!(
                    "lean-toolchain pins {toolchain_id}, outside the lean-rs supported window {window}; \
                     nearest supported: {nearest}. Pin a supported toolchain (or bump lean-rs) and reopen."
                )));
            }
            Readiness::Stale { toolchain, install_cmd } => {
                return Err(ServerError::BadProject(format!(
                    "worker for {toolchain} was built against a different lean.h than the toolchain now \
                     provides (header drift); rebuild it: {install_cmd}"
                )));
            }
            Readiness::Incompatible {
                toolchain,
                worker_protocol,
                host_protocol,
                install_cmd: _,
            } => {
                return Err(ServerError::IncompatibleWorker {
                    message: format!(
                        "worker for {toolchain} uses protocol {worker_protocol}, but this host requires \
                         protocol {host_protocol}"
                    ),
                    recovery_command: "lean-host-mcp install-worker --auto".to_owned(),
                });
            }
            Readiness::Unusable {
                toolchain,
                detail,
                install_cmd,
            } => {
                return Err(ServerError::BadProject(format!(
                    "worker for {toolchain} failed its runtime smoke test ({detail}); the toolchain's \
                     libleanshared is ABI-incompatible with this lean-rs build and cannot be served. \
                     Pin a supported toolchain the host can run, or rebuild lean-rs and reinstall: {install_cmd}"
                )));
            }
            Readiness::NotInstalled { toolchain, install_cmd } => {
                return Err(ServerError::BadProject(format!(
                    "no worker binary for toolchain {toolchain}; run: {install_cmd}"
                )));
            }
            Readiness::ToolchainNotInstalled { toolchain, elan_dir } => {
                return Err(ServerError::BadProject(format!(
                    "elan toolchain {toolchain} is not installed (expected {})",
                    elan_dir.display()
                )));
            }
        };
        tracing::debug!(
            toolchain = %toolchain_id,
            worker = %worker_path.display(),
            sysroot = %lean_sysroot.display(),
            "resolved ready worker binary"
        );
        let config = Self {
            lake_root: meta.canonical_root.clone(),
            manifest_hash: meta.manifest_hash.clone(),
            toolchain_label: meta.toolchain.clone(),
            worker_path,
            lean_sysroot,
            session_id,
            runtime,
            healthy,
            artifact_roots: lake_meta::artifact_roots(&meta.canonical_root),
            lean_max_memory_kib: runtime_config.lean_max_memory_kib(),
            request_timeout_millis: runtime_config.request_timeout_millis(),
            mailbox_capacity: runtime_config.mailbox_capacity(),
            max_restarts_per_window: runtime_config.max_restarts_per_window(),
            restart_window: runtime_config.restart_window(),
            import_residue_budget_bytes: runtime_config.import_residue_budget_bytes(),
            import_residue_soft_bytes: runtime_config.import_residue_soft_bytes(),
            session_pool_capacity: runtime_config.session_pool_capacity(),
            tokio_handle: tokio::runtime::Handle::try_current().ok(),
            toolchain_advisories: open_warnings.clone(),
        };
        Ok((config, open_warnings))
    }
}

/// One import profile the child may be holding, and the build state it was
/// holding it at.
struct ImportProfileStamp {
    fingerprint: String,
    /// The profile's imports, kept so a pre-warm after an idle cycle can reopen
    /// it. The fingerprint is `imports.join("\n")` and could be split back, but
    /// that makes the pre-warm depend on the fingerprint's spelling; this list is
    /// what the child was actually asked for.
    imports: Vec<String>,
    /// Newest `.olean` mtime among that profile's imports, sampled when it was
    /// last served. `None` for an unbuilt project, where there is no mtime to
    /// compare against.
    artifact_stamp: Option<std::time::SystemTime>,
    /// When this profile was last served, so an idle cycle only pre-warms a
    /// profile a returning caller is plausibly still working in.
    last_served: Instant,
}

struct ProjectActorState {
    config: ActorConfig,
    handle: LeanWorkerHostHandle,
    worker_generation_base: u64,
    last_restart: Option<RuntimeRestartEvent>,
    /// The import profiles the live worker child may still be holding, MRU at
    /// the back, bounded by [`WORKER_MAX_IMPORTS`] so it mirrors the child's own
    /// session pool.
    ///
    /// A *list* because the child pools sessions: it holds several imported
    /// environments at once, so "the profile the live session was opened with"
    /// is no longer a single value, and a stamp recorded for one profile stays
    /// relevant while another is served.
    imports_seen: Vec<ImportProfileStamp>,
    profile_switch_count: u64,
    last_rss_kib: Option<u64>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    abnormal_restart_times: VecDeque<Instant>,
    restart_stats: RestartStats,
    /// Whether this quiet period has already taken its idle cycle.
    ///
    /// One is all a quiet period can profitably take. The pre-warm that follows
    /// a cycle re-imports, which puts the fresh child's residue back above zero
    /// and, under a budget small enough, straight back over the soft threshold —
    /// so without this the actor would cycle, pre-warm, and cycle again every
    /// grace interval, discarding each pre-warm with nothing served in between.
    /// Cleared by the next served call, which is what makes the *next* cycle
    /// worth something.
    cycled_while_idle: bool,
}

impl ProjectActorState {
    fn handle_message(&mut self, message: ProjectMessage) {
        // A call was served, so the next quiet period earns a fresh idle cycle.
        self.cycled_while_idle = false;
        match message {
            ProjectMessage::ModuleQuery {
                meta,
                source,
                query,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.process_module_query_with_imports(imports, &source, &query, &options, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::ModuleQueryBatch {
                meta,
                source,
                selectors,
                budgets,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.process_module_query_batch_with_imports(
                        imports, &source, &selectors, &budgets, &options, None, None,
                    )
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationInspection { meta, request, reply } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.inspect_declaration_with_imports(imports, &request, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationSearch { meta, requests, reply } => {
                // One session for the whole group. `search_declarations_with_imports`
                // opens and drops a session per call, and every open re-imports,
                // so the per-search cost here is a worker round trip rather than
                // an import. The session borrows `&mut handle` and dies inside
                // the closure, so nothing escapes the actor's stack frame.
                let result = self.run_job(meta, |handle, imports| {
                    let mut session = handle.open_session_with_imports(imports, None, None)?;
                    requests
                        .iter()
                        .map(|request| session.search_declarations(request, None, None))
                        .collect()
                });
                let _ = reply.send(result);
            }
            ProjectMessage::ProofAttempt {
                meta,
                request,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.attempt_proof_with_imports(imports, &request, &options, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationVerification {
                meta,
                request,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.verify_declaration_with_imports(imports, &request, &options, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationVerificationBatch {
                meta,
                request,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.verify_declaration_batch_with_imports(imports, &request, &options, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::SemanticProofSearch { meta, request, reply } => {
                let result = self.run_semantic_job(meta, &request);
                let _ = reply.send(result);
            }
        }
    }

    fn run_job<R>(
        &mut self,
        meta: JobMeta,
        job: impl Fn(&mut LeanWorkerHostHandle, Vec<String>) -> std::result::Result<R, LeanWorkerError>,
    ) -> Result<ProjectCall<R>> {
        // Runs on the project's dedicated actor thread (no async), so an entered
        // span is correct and ties every nested worker/recycle log to this call.
        let _span = tracing::debug_span!(
            "job",
            session_id = %self.config.session_id,
            imports = meta.imports.len(),
            queue_wait_millis = millis_u64(meta.queued_at.elapsed()),
        )
        .entered();
        let queue_wait_millis = millis_u64(meta.queued_at.elapsed());
        let generation_before = self.observed_generation();
        self.note_import_profile_switch(&meta);
        let mut call_restart: Option<RuntimeRestartEvent> = self.cycle_if_imports_rebuilt(&meta)?;
        let mut lifecycle_baseline = self.handle.lifecycle_snapshot();

        let max_retries = meta.retry_policy.retries();
        let mut retry_count = 0_u32;
        loop {
            match job(&mut self.handle, meta.imports.clone()) {
                Ok(value) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    // Sampled for `RuntimeFacts.rss_kib`, which is reporting
                    // only: no threshold reads it, and nothing restarts on it.
                    self.last_rss_kib = self.handle.rss_kib().or(self.last_rss_kib);
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    tracing::debug!(
                        retry_count,
                        rss_kib = ?runtime.rss_kib,
                        worker_generation = runtime.worker_generation,
                        "job complete"
                    );
                    self.publish_runtime(&runtime);
                    return Ok(ProjectCall::new(value, runtime));
                }
                Err(err) if worker_error_is_recoverable_death(&err) && retry_count < max_retries => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)?;
                    let first_reason = err.to_string();
                    call_restart =
                        Some(self.rebuild_after_worker_death(first_reason, worker_death_cause(&err), &meta)?);
                    lifecycle_baseline = self.handle.lifecycle_snapshot();
                    retry_count = retry_count.saturating_add(1);
                }
                Err(err) if worker_error_is_recoverable_death(&err) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let reason = format!("worker_died_after_retry: {err}");
                    let generation = self.observed_generation();
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        reason,
                        true,
                        generation > generation_before,
                        Some(worker_death_cause(&err)),
                        None,
                        None,
                    ));
                }
                Err(err) if worker_error_is_session_missing(&err) && retry_count < max_retries => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)?;
                    call_restart = Some(self.rebuild_after_worker_death(
                        format!("session_missing: {err}"),
                        RestartCause::SessionMissing,
                        &meta,
                    )?);
                    lifecycle_baseline = self.handle.lifecycle_snapshot();
                    retry_count = retry_count.saturating_add(1);
                }
                Err(err) if worker_error_is_session_missing(&err) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let generation = self.observed_generation();
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        format!("session_missing: {err}"),
                        true,
                        generation > generation_before,
                        Some(RestartCause::SessionMissing),
                        None,
                        None,
                    ));
                }
                Err(LeanWorkerError::RssHardLimitExceeded {
                    operation,
                    current_kib,
                    limit_kib,
                    ..
                }) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        format!(
                            "rss_hard_limit_exceeded operation={operation} current_kib={current_kib} limit_kib={limit_kib}"
                        ),
                        false,
                        true,
                        Some(RestartCause::RssHardLimit),
                        Some(limit_kib),
                        None,
                    ));
                }
                Err(err) if matches!(err, LeanWorkerError::Timeout { .. }) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let generation = self.observed_generation();
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        format!("timeout: {err}"),
                        true,
                        generation > generation_before,
                        Some(RestartCause::Timeout),
                        None,
                        None,
                    ));
                }
                Err(err) => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)?;
                    return Err(map_worker_err(err));
                }
            }
        }
    }

    fn run_semantic_job(
        &self,
        meta: JobMeta,
        request: &SemanticProofSearchRequest,
    ) -> Result<ProjectCall<SemanticProofSearchResult>> {
        let _span = tracing::debug_span!(
            "semantic_job",
            session_id = %self.config.session_id,
            imports = meta.imports.len(),
            queue_wait_millis = millis_u64(meta.queued_at.elapsed()),
        )
        .entered();
        let queue_wait_millis = millis_u64(meta.queued_at.elapsed());
        let generation_before = self.observed_generation();
        let mut capability = self.open_semantic_capability(&meta)?;
        let result = {
            let mut session = capability
                .open_session_with_imports(meta.imports.clone(), None, None)
                .map_err(map_worker_err)?;
            crate::semantic_search::run_semantic_proof_search(&mut session, request)
        };
        let runtime = self.runtime_facts(&meta, generation_before, 0, queue_wait_millis, None);
        self.publish_runtime(&runtime);
        result.map(|value| ProjectCall::new(value, runtime))
    }

    /// Spawn the semantic-search child for one call. The child is dropped when
    /// the caller's `LeanWorkerCapability` goes out of scope.
    ///
    /// **Keeping it resident between calls has now been measured twice, and
    /// rejected twice for different reasons.**
    ///
    /// The first measurement — 2.71 s per-call spawn versus 3.30 s resident on
    /// `benches/search_for_proof.rs`, a 15–22% regression — was taken against a
    /// worker child that re-imported on every `open_session_with_imports`.
    /// Residency saved the process spawn while letting one child accumulate
    /// every call's unreclaimable import, and the accumulation cost more. That
    /// reason is now obsolete: a session whose imports match the live one is
    /// reused.
    ///
    /// So it was re-implemented — resident on the actor state, evicted by
    /// import fingerprint — and re-measured on the same bench: **1.35 s
    /// per-call spawn versus 1.49 s resident** (p = 0.11, confidence intervals
    /// overlapping). No significant improvement, with the point estimate again
    /// slightly worse, so the simpler per-call spawn stands. The spawn it would
    /// save is smaller than `benches/worker_cold_spawn.rs`'s 845 ms suggests:
    /// that figure is the *main* worker's cold spawn plus a first inspect, and
    /// the semantic child imports a different, smaller set.
    ///
    /// Revisit only with a workload where the semantic child's spawn is a
    /// measured majority of the call — and revert again unless the bench moves.
    fn open_semantic_capability(&self, meta: &JobMeta) -> Result<lean_rs_worker_parent::LeanWorkerCapability> {
        let runtime = semantic_runtime(&self.config.toolchain_label, &self.config.lean_sysroot).map_err(|err| {
            self.worker_unavailable_for(
                meta,
                format!("semantic runtime build failed for this toolchain: {err}"),
                true,
                false,
                None,
                None,
                None,
            )
        })?;
        semantic_capability_builder(&self.config, &runtime.built)?
            .open()
            .map_err(|err| {
                self.worker_unavailable_for(
                    meta,
                    format!(
                        "semantic capability open failed for this toolchain: {}",
                        map_worker_err(err)
                    ),
                    true,
                    false,
                    None,
                    None,
                    None,
                )
            })
    }

    /// Drop the live session when a `.olean` it imported has been rebuilt.
    ///
    /// A worker session's environment is a snapshot taken at import: the child
    /// reuses a session whose imports match, so `.olean` files written after
    /// that import are invisible to it. Before the child learned to reuse,
    /// every call re-imported, and that accident is what kept a mid-session
    /// `lake build` from being served stale answers. This restores the property
    /// deliberately, at k `stat` calls per job rather than a full import.
    ///
    /// Checked against the stamp recorded for *this* import profile, whichever
    /// profile ran in between. Until the child pooled sessions, a differing
    /// profile always re-imported, so only the immediately preceding profile
    /// could be stale and one remembered stamp sufficed. A pooled child parks
    /// the old environment instead, so a profile served three calls ago can be
    /// restored without importing — and would be served from `.olean` files
    /// rebuilt since. Recycling before the job means the job then runs on a
    /// fresh worker, which is why `artifacts_rebuilt` is not an execution taint.
    ///
    /// The stamp is read just before the session opens, not after, so a build
    /// that lands inside that window is attributed to the new session and the
    /// following call sees no advance. Closing it would cost a second stat pass
    /// per job to catch a race measured in milliseconds against a `lake build`
    /// measured in seconds; the artifact facts in `trust` still report the
    /// staleness for the modules a call actually touches.
    fn cycle_if_imports_rebuilt(&mut self, meta: &JobMeta) -> Result<Option<RuntimeRestartEvent>> {
        let stamp = lake_meta::import_artifact_stamp(&self.config.artifact_roots, &meta.imports);
        // Recorded unconditionally: this is the stamp of the artifacts the
        // session about to serve the job will have imported, whether that
        // session is a pooled one, the live one, or the replacement opened
        // below.
        let previous = self.note_import_profile(&meta.import_fingerprint, &meta.imports, stamp);
        let (Some(previous), Some(current)) = (previous, stamp) else {
            return Ok(None);
        };
        if current <= previous {
            return Ok(None);
        }
        let reason = format!("artifacts_rebuilt imports={} newest_olean_advanced", meta.imports.len());
        self.record_restart_or_stop(RestartCause::ArtifactsRebuilt, &reason)
            .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        self.handle.cycle().map_err(map_worker_err)?;
        self.forget_pooled_import_profiles();
        let event = restart_event(
            RestartCause::ArtifactsRebuilt,
            reason,
            self.observed_generation(),
            self.last_rss_kib,
            None,
        );
        self.record_restart(event.clone());
        Ok(Some(event))
    }

    /// The profile the most recent job ran with, or `None` before the first job.
    ///
    /// Reported as the `import_profile` runtime fact and used to count switches.
    /// Both want "most recent", which is the back of the MRU list.
    fn current_import_profile(&self) -> Option<&str> {
        self.imports_seen.last().map(|held| held.fingerprint.as_str())
    }

    /// Record that `fingerprint` is now the most recently served profile,
    /// carrying `stamp`, and return the stamp the child last held it at.
    ///
    /// `None` covers both "the child cannot be holding this profile" and "it
    /// held it over an unbuilt project", deliberately: neither gives an mtime
    /// to compare the incoming one against, so the caller treats them alike.
    fn note_import_profile(
        &mut self,
        fingerprint: &str,
        imports: &[String],
        stamp: Option<std::time::SystemTime>,
    ) -> Option<std::time::SystemTime> {
        if let Some(index) = self
            .imports_seen
            .iter()
            .position(|held| held.fingerprint == fingerprint)
        {
            let mut held = self.imports_seen.remove(index);
            let previous = held.artifact_stamp;
            held.artifact_stamp = stamp;
            held.last_served = Instant::now();
            self.imports_seen.push(held);
            return previous;
        }
        // Bounded by the child's own session-pool capacity, so the parent's
        // picture of what the child holds cannot outgrow what it can hold.
        while self.imports_seen.len() >= self.config.session_pool_capacity {
            self.imports_seen.remove(0);
        }
        self.imports_seen.push(ImportProfileStamp {
            fingerprint: fingerprint.to_owned(),
            imports: imports.to_vec(),
            artifact_stamp: stamp,
            last_served: Instant::now(),
        });
        None
    }

    /// Cycle the child now, between requests, if imports have retained more than
    /// the soft share of the residue budget.
    ///
    /// The recycle itself is not avoidable — residue is unreclaimable, so a
    /// child that has imported enough must be replaced eventually — but *when*
    /// it lands is. Run reactively it costs the next caller a ~730 ms respawn on
    /// top of their import; run here it costs nobody anything, because every
    /// reply has already been sent and the mailbox is empty.
    ///
    /// A cheap recycle is also newly worth taking. `.olean` compacted regions
    /// are position-dependent, so the *first* import in a fresh process maps
    /// them at their preferred addresses and retains almost nothing — 0.29 GB of
    /// footprint for a 2.2 GB closure, against 2–4.5 GB for the same import
    /// performed later in a process. A fresh child is not merely emptier, its
    /// next import is far cheaper.
    ///
    /// Having cycled, it re-imports the most recently served profile into the
    /// fresh child, so the next caller does not pay for the emptiness this
    /// created. Bounded by recency: a profile nobody has touched in
    /// [`PREWARM_RECENCY`] is a guess, and a wrong guess costs a real import.
    /// The bounded wait to arm before the next receive, or `None` to block.
    ///
    /// `None` is the normal answer and costs nothing: under the soft budget
    /// there is no cycle to schedule, so the actor blocks on its mailbox with no
    /// timer and no extra wakeups. It is also the answer in a synchronous test
    /// context, where there is no runtime to borrow — the mailbox-empty check in
    /// the loop still fires, so idle cycling degrades rather than disappearing.
    fn idle_grace(&self) -> Option<(&tokio::runtime::Handle, Duration)> {
        let handle = self.config.tokio_handle.as_ref()?;
        self.over_soft_budget().map(|_residue| (handle, QUIESCENCE_GRACE))
    }

    /// The generation's retained residue, if it is worth cycling for.
    ///
    /// Two conditions, not one. The threshold is the obvious half. The other is
    /// that the generation has actually retained something: a child that has
    /// imported nothing has nothing a cycle could reclaim, so replacing it is
    /// pure cost — and with a soft budget of zero (a budget small enough that
    /// 60% of it rounds to nothing) the threshold alone would be satisfied by
    /// every freshly spawned child, cycling forever and importing never.
    fn over_soft_budget(&self) -> Option<u64> {
        if self.cycled_while_idle {
            return None;
        }
        let residue = self.handle.lifecycle_snapshot().import_residue_bytes;
        (residue > 0 && residue >= self.config.import_residue_soft_bytes).then_some(residue)
    }

    /// Cycle over the soft budget, having *demonstrated* that the project is
    /// quiet: the mailbox stayed empty for a whole [`QUIESCENCE_GRACE`]. That is
    /// what earns the pre-warm, which is a multi-second import at Mathlib scale
    /// and would land on the next caller's latency if taken mid-burst.
    fn cycle_on_quiescence(&mut self) {
        self.cycle_over_soft_budget(true);
    }

    /// Cycle over the soft budget with nothing more than an empty mailbox to go
    /// on. Used only where no tokio handle is available to arm the quiescence
    /// timer — a synchronous embedder or test — so idle cycling degrades to
    /// moving the respawn off the request path rather than disappearing. No
    /// pre-warm: an empty mailbox one instant after a reply is not evidence of a
    /// pause, and a wrong guess costs a real import.
    fn cycle_after_reply(&mut self) {
        self.cycle_over_soft_budget(false);
    }

    fn cycle_over_soft_budget(&mut self, quiesced: bool) {
        let Some(residue) = self.over_soft_budget() else {
            return;
        };
        // Captured before the cycle, because the cycle invalidates every
        // remembered profile and this is the one worth paying to get back.
        let prewarm = quiesced
            .then(|| {
                self.imports_seen
                    .last()
                    .filter(|held| held.last_served.elapsed() <= PREWARM_RECENCY)
                    .map(|held| (held.fingerprint.clone(), held.imports.clone()))
            })
            .flatten();
        let reason = format!(
            "import_residue_idle residue_mib={} soft_mib={} limit_mib={}",
            residue / (1024 * 1024),
            self.config.import_residue_soft_bytes / (1024 * 1024),
            self.config.import_residue_budget_bytes / (1024 * 1024),
        );
        if let Err(err) = self.handle.cycle() {
            // Nothing is owed to a caller here — there is no call in flight —
            // and the reactive bound still stands, so a failed idle cycle is a
            // missed optimisation rather than an error to propagate.
            tracing::debug!(error = %err, "idle worker cycle failed; the reactive residue bound still applies");
            return;
        }
        // A full clear, not `forget_pooled_import_profiles`: that one preserves
        // the profile of the job currently being served, and there is no such
        // job here. The fresh child holds nothing.
        self.imports_seen.clear();
        // From here the cycle has happened, so this quiet period is spent
        // whether or not the pre-warm below succeeds.
        self.cycled_while_idle = true;
        let event = restart_event(
            RestartCause::ImportResidueIdle,
            reason,
            self.observed_generation(),
            self.last_rss_kib,
            None,
        );
        self.record_restart(event.clone());
        self.publish_runtime(&RuntimeFacts {
            worker_generation: event.worker_generation,
            worker_restarted: true,
            retry_count: 0,
            queue_wait_millis: 0,
            call_restart: None,
            last_restart: Some(event),
            rss_kib: self.last_rss_kib,
            worker_lanes: 1,
            import_profile: None,
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restart_stats.total,
            restarts_by_cause: self.restart_stats.by_cause.clone(),
            import_residue_bytes: Some(self.handle.lifecycle_snapshot().import_residue_bytes),
            import_residue_limit_bytes: Some(self.config.import_residue_budget_bytes),
        });

        // The cycle relocated the respawn; this relocates the import, which at
        // Mathlib scale is the expensive half by an order of magnitude.
        let Some((fingerprint, imports)) = prewarm else { return };
        match self.handle.open_session_with_imports(imports.clone(), None, None) {
            Ok(_session) => {
                // Record it the same way a served job would, so the *next* call
                // on this profile compares artifact stamps against the import
                // that actually happened rather than treating it as unseen.
                let stamp = lake_meta::import_artifact_stamp(&self.config.artifact_roots, &imports);
                let _previous = self.note_import_profile(&fingerprint, &imports, stamp);
            }
            Err(err) => {
                // Same posture as a failed cycle: nobody is waiting, and the
                // next real call imports this itself.
                tracing::debug!(error = %err, "idle pre-warm failed; the next call will import normally");
            }
        }
    }

    /// Forget every pooled profile except the one the current job is serving.
    ///
    /// Called after the child is replaced: the new child holds nothing its
    /// predecessor held, so every remembered stamp but one describes an
    /// environment that no longer exists. The exception is the current job's
    /// profile, which the caller records before the replacement and the job
    /// then re-imports into the fresh child — dropping it too would blind the
    /// *next* call to a rebuild that lands right after this one.
    fn forget_pooled_import_profiles(&mut self) {
        if let Some(current) = self.imports_seen.pop() {
            self.imports_seen.clear();
            self.imports_seen.push(current);
        }
    }

    /// Count a change of import profile for telemetry.
    ///
    /// This used to also cycle the worker when RSS was above a soft ceiling, on
    /// the theory that an import switch would otherwise hold two imported
    /// environments at once. It now does — the child pools them deliberately,
    /// at a measured 30–50 MB per extra live environment against the ~1 GiB an
    /// import costs. What bounds retained bytes is
    /// [`WORKER_IMPORT_RESIDUE_CEILING_BYTES`]; what backstops the heap above it
    /// is [`lean_max_memory_kib_for`]. Neither is a process-level RSS reading,
    /// which mostly measured mmapped `.olean` pages.
    fn note_import_profile_switch(&mut self, meta: &JobMeta) {
        let switched = self
            .current_import_profile()
            .is_some_and(|previous| previous != meta.import_fingerprint);
        if switched {
            self.profile_switch_count = self.profile_switch_count.saturating_add(1);
        }
    }

    fn rebuild_after_worker_death(
        &mut self,
        reason: String,
        cause: RestartCause,
        meta: &JobMeta,
    ) -> Result<RuntimeRestartEvent> {
        self.record_restart_or_stop(cause, &reason)
            .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        let next_generation = self.observed_generation().saturating_add(1);
        let (handle, _) = open_worker(&self.config, false)?;
        self.handle = handle;
        self.worker_generation_base = next_generation;
        self.forget_pooled_import_profiles();
        self.last_rss_kib = self.handle.rss_kib().or(self.last_rss_kib);
        let event = restart_event(cause, reason, self.observed_generation(), self.last_rss_kib, None);
        self.record_restart(event.clone());
        Ok(event)
    }

    fn account_lifecycle_restarts_since(
        &mut self,
        before: &LeanWorkerLifecycleSnapshot,
        meta: &JobMeta,
    ) -> Result<Option<RuntimeRestartEvent>> {
        let after = self.handle.lifecycle_snapshot();
        let restarted = after.restarts.saturating_sub(before.restarts);
        if restarted == 0 {
            self.last_rss_kib = after.last_rss_kib.or(self.last_rss_kib);
            return Ok(None);
        }
        let (cause, reason) = after.last_restart_reason.as_ref().map_or_else(
            || (RestartCause::WorkerInternal, "worker_internal_restart".to_owned()),
            |reason| (restart_cause_from_worker(reason), restart_reason_text(reason)),
        );
        for _ in 0..restarted {
            self.record_restart_or_stop(cause, &reason)
                .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        }
        // The supervisor replaced the child under us — most often on its own
        // `max_imports` policy, which is exactly the pooled-profile ceiling.
        self.forget_pooled_import_profiles();
        self.last_rss_kib = after.last_rss_kib.or(self.last_rss_kib);
        let event = restart_event(cause, reason, self.observed_generation(), self.last_rss_kib, None);
        self.record_restart(event.clone());
        Ok(Some(event))
    }

    /// The single place a recycle becomes observable: tally it for frequency
    /// reporting, log it at a signal-appropriate level, and store it as the
    /// latest event. Every restart path funnels through here, so adding one is
    /// a single call. Kept distinct from [`Self::record_restart_or_stop`], which
    /// owns the orthogonal sliding-window health *policy*.
    fn record_restart(&mut self, event: RuntimeRestartEvent) {
        self.restart_stats.observe(&event.cause);
        log_restart(&event, self.restart_stats.total);
        self.last_restart = Some(event);
    }

    fn record_restart_or_stop(
        &mut self,
        cause: RestartCause,
        reason: &str,
    ) -> std::result::Result<(), RestartLimitExceeded> {
        if !cause.counts_toward_restart_limit() {
            return Ok(());
        }
        let now = Instant::now();
        while self
            .abnormal_restart_times
            .front()
            .is_some_and(|seen| now.saturating_duration_since(*seen) > self.config.restart_window)
        {
            self.abnormal_restart_times.pop_front();
        }
        if self.abnormal_restart_times.len() >= self.config.max_restarts_per_window {
            self.config.healthy.store(false, Ordering::Release);
            tracing::warn!(
                cause = cause.as_str(),
                restarts_in_window = self.abnormal_restart_times.len(),
                window_millis = millis_u64(self.config.restart_window),
                "restart limit exceeded; marking project unhealthy"
            );
            let message = format!(
                "restart_limit_exceeded after {} restarts in {:?}; latest: {reason}",
                self.config.max_restarts_per_window, self.config.restart_window
            );
            let event = restart_event(
                cause,
                message.clone(),
                self.observed_generation(),
                self.last_rss_kib,
                None,
            );
            self.record_restart(event.clone());
            self.publish_runtime(&RuntimeFacts {
                worker_generation: self.observed_generation(),
                worker_restarted: false,
                retry_count: MAX_JOB_RETRIES,
                queue_wait_millis: 0,
                call_restart: None,
                last_restart: Some(event),
                rss_kib: self.last_rss_kib,
                worker_lanes: 1,
                import_profile: self.current_import_profile().map(str::to_owned),
                profile_switch_count: self.profile_switch_count,
                restarts_total: self.restart_stats.total,
                restarts_by_cause: self.restart_stats.by_cause.clone(),
                import_residue_bytes: Some(self.handle.lifecycle_snapshot().import_residue_bytes),
                import_residue_limit_bytes: self.config.import_residue_budget_bytes.into(),
            });
            return Err(RestartLimitExceeded {
                message,
                cause,
                restarts_in_window: self.abnormal_restart_times.len() as u64,
                window_millis: millis_u64(self.config.restart_window),
            });
        }
        self.abnormal_restart_times.push_back(now);
        Ok(())
    }

    fn observed_generation(&self) -> u64 {
        self.worker_generation_base
            .saturating_add(self.handle.lifecycle_snapshot().worker_generation)
    }

    fn runtime_facts(
        &self,
        meta: &JobMeta,
        generation_before: u64,
        retry_count: u32,
        queue_wait_millis: u64,
        call_restart: Option<RuntimeRestartEvent>,
    ) -> RuntimeFacts {
        let generation = self.observed_generation();
        let snapshot = self.handle.lifecycle_snapshot();
        RuntimeFacts {
            worker_generation: generation,
            worker_restarted: call_restart.is_some() || generation > generation_before,
            retry_count,
            queue_wait_millis,
            call_restart,
            last_restart: self.last_restart.clone(),
            rss_kib: snapshot.last_rss_kib.or(self.last_rss_kib),
            worker_lanes: 1,
            import_profile: Some(meta.import_fingerprint.clone()),
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restart_stats.total,
            restarts_by_cause: self.restart_stats.by_cause.clone(),
            import_residue_bytes: Some(snapshot.import_residue_bytes),
            import_residue_limit_bytes: snapshot.import_residue_limit_bytes,
        }
    }

    fn publish_runtime(&self, runtime: &RuntimeFacts) {
        *self.runtime.lock() = RuntimeSnapshot {
            worker_generation: runtime.worker_generation,
            last_restart: runtime.last_restart.clone().or_else(|| runtime.call_restart.clone()),
            rss_kib: runtime.rss_kib,
            import_profile: runtime.import_profile.clone(),
            profile_switch_count: runtime.profile_switch_count,
            restarts_total: runtime.restarts_total,
            restarts_by_cause: runtime.restarts_by_cause.clone(),
            import_residue_bytes: runtime.import_residue_bytes,
            import_residue_limit_bytes: runtime.import_residue_limit_bytes,
        };
    }

    fn worker_unavailable_for(
        &self,
        meta: &JobMeta,
        reason: String,
        retryable: bool,
        worker_restarted: bool,
        cause: Option<RestartCause>,
        limit_kib: Option<u64>,
        retry_after_millis: Option<u64>,
    ) -> ServerError {
        let snapshot = self.runtime.lock().facts();
        ServerError::worker_unavailable(WorkerUnavailable {
            retryable,
            worker_restarted,
            project_root: self.config.lake_root.to_string_lossy().into_owned(),
            project_hash: self.config.manifest_hash.clone(),
            imports: meta.imports.clone(),
            session_id: self.config.session_id.clone(),
            lean_toolchain: self.config.toolchain_label.clone(),
            worker_generation: self.observed_generation(),
            restart_cause: cause.map(|cause| cause.as_str().to_owned()),
            rss_kib: self.last_rss_kib,
            limit_kib,
            retry_after_millis,
            restarts_in_window: Some(self.abnormal_restart_times.len() as u64),
            window_millis: Some(millis_u64(self.config.restart_window)),
            runtime: snapshot,
            reason,
            toolchain_advisories: self.config.toolchain_advisories.clone(),
        })
    }

    fn shutdown_unavailable(&self, meta: &JobMeta, reason: &'static str) -> ServerError {
        self.worker_unavailable_for(meta, reason.to_owned(), true, false, None, None, None)
    }

    fn restart_limit_error(&self, imports: &[String], limit: RestartLimitExceeded) -> ServerError {
        let snapshot = self.runtime.lock().facts();
        ServerError::worker_unavailable(WorkerUnavailable {
            retryable: false,
            worker_restarted: false,
            project_root: self.config.lake_root.to_string_lossy().into_owned(),
            project_hash: self.config.manifest_hash.clone(),
            imports: imports.to_vec(),
            session_id: self.config.session_id.clone(),
            lean_toolchain: self.config.toolchain_label.clone(),
            worker_generation: self.observed_generation(),
            reason: limit.message,
            restart_cause: Some(limit.cause.as_str().to_owned()),
            rss_kib: self.last_rss_kib,
            limit_kib: None,
            retry_after_millis: Some(limit.window_millis),
            restarts_in_window: Some(limit.restarts_in_window),
            window_millis: Some(limit.window_millis),
            runtime: snapshot,
            toolchain_advisories: self.config.toolchain_advisories.clone(),
        })
    }
}

struct RestartLimitExceeded {
    message: String,
    cause: RestartCause,
    restarts_in_window: u64,
    window_millis: u64,
}

fn actor_main(
    config: ActorConfig,
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<ProjectMessage>), ServerError>>,
) {
    let (handle, runtime_toolchain) = match open_worker(&config, true) {
        Ok(value) => value,
        Err(err) => {
            let _ = init_reply.send(Err(err));
            return;
        }
    };
    let mut state = ProjectActorState {
        config: config.clone(),
        handle,
        worker_generation_base: 1,
        last_restart: None,
        imports_seen: Vec::new(),
        profile_switch_count: 0,
        last_rss_kib: None,
        runtime: Arc::clone(&config.runtime),
        abnormal_restart_times: VecDeque::new(),
        restart_stats: RestartStats::default(),
        cycled_while_idle: false,
    };

    // Once per project, at open. Below the floor the budget can be smaller than
    // a single import's residue, at which point the policy degenerates into
    // recycling the child before nearly every import — strictly worse than the
    // count bound it replaced, and not something the response envelope makes
    // obvious, since each of those cycles is individually well-formed.
    if config.import_residue_budget_bytes < WORKER_IMPORT_RESIDUE_FLOOR_BYTES {
        tracing::warn!(
            budget_mib = config.import_residue_budget_bytes / (1024 * 1024),
            floor_mib = WORKER_IMPORT_RESIDUE_FLOOR_BYTES / (1024 * 1024),
            "import residue budget is below the floor one Mathlib-scale import needs; \
             raise runtime.worker_import_residue_budget_mib if workers recycle constantly"
        );
    }
    // Only reachable when an operator sets `runtime.lean_max_memory_kib`
    // explicitly — the derived default cannot invert. Worth a warning rather
    // than a clamp because it is their machine and their call, but it is almost
    // never what they meant: below the budget the ceiling always fires first,
    // and it fires as a child abort rather than a clean recycle.
    if config.lean_max_memory_kib.saturating_mul(1024) <= config.import_residue_budget_bytes {
        tracing::warn!(
            lean_max_memory_mib = config.lean_max_memory_kib / 1024,
            budget_mib = config.import_residue_budget_bytes / (1024 * 1024),
            "Lean heap ceiling is at or below the import residue budget, so it will abort the \
             child before the budget can recycle it cleanly; raise runtime.lean_max_memory_kib \
             above the budget or leave it unset to track it"
        );
    }

    let (tx, mut rx) = mpsc::channel::<ProjectMessage>(config.mailbox_capacity);
    if init_reply.send(Ok((runtime_toolchain, tx))).is_err() {
        return;
    }

    // Every reply is sent inside `handle_message` before it returns, so by the
    // time control reaches the bottom of this loop the client already has its
    // answer. That is what makes an idle cycle free: it happens after the work
    // it would otherwise have delayed.
    loop {
        let message = match state.idle_grace() {
            None => rx.blocking_recv(),
            // Over the soft budget: wait a bounded moment for the next call
            // instead of forever, so a genuine pause becomes a chance to
            // recycle. `block_on` wraps the *receive* only — no Lean session
            // exists at this point, let alone crosses it.
            Some((handle, grace)) => match handle.block_on(async { tokio::time::timeout(grace, rx.recv()).await }) {
                Ok(message) => message,
                Err(_elapsed) => {
                    state.cycle_on_quiescence();
                    continue;
                }
            },
        };
        let Some(message) = message else { break };
        if !config.healthy.load(Ordering::Acquire) {
            message.reject(&state, "project_shutting_down");
            continue;
        }
        state.handle_message(message);
        // Only where `idle_grace` can never arm. With a runtime available the
        // quiescence tick above does this job two seconds later and pre-warms as
        // well, so cycling here would spend the generation on the strictly worse
        // of the two paths.
        if config.tokio_handle.is_none() && rx.is_empty() {
            state.cycle_after_reply();
        }
    }

    match state.handle.shutdown() {
        Ok(report) => {
            tracing::debug!(
                outcome = ?report.outcome,
                elapsed_millis = millis_u64(report.elapsed),
                wait_millis = millis_u64(report.wait_elapsed),
                "project worker shutdown complete"
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "project worker shutdown failed");
        }
    }
}

fn open_worker(config: &ActorConfig, preflight: bool) -> Result<(LeanWorkerHostHandle, String)> {
    let builder = worker_builder(config);
    if preflight {
        let report = builder.check();
        if let Some(first) = report.first_error() {
            return Err(ServerError::BadProject(format!(
                "{}: {}",
                first.code(),
                first.message()
            )));
        }
    }
    // No `RssHardLimitExceeded` arm: this server no longer sets
    // `rss_hard_limit`, so the supervisor cannot produce one. See
    // `lean_max_memory_kib_for`.
    let handle = builder.open().map_err(map_worker_err)?;
    let runtime_toolchain = handle
        .runtime_metadata()
        .lean_version
        .unwrap_or_else(|| config.toolchain_label.clone());
    Ok((handle, runtime_toolchain))
}

/// One policy restart, on the one quantity that accumulates without bound: the
/// bytes an import retains and the child cannot give back.
///
/// `disabled()`'s own doc warns that a long-running host wants
/// `memory_bounded`, "because fresh imports retain Lean process-global state
/// until the child exits" — session reuse made that true per *import* rather
/// than per *call*, and [`WORKER_IMPORT_RESIDUE_CEILING_BYTES`] explains why the
/// bound is denominated in the bytes those imports retain rather than in their
/// number. [`WORKER_MAX_IMPORTS_BACKSTOP`] rides behind it as the guard against
/// a child that reports no residue at all.
///
/// Still no `max_rss_kib`, no `max_requests`, no `rss_hard_limit`. The Lean heap
/// is backstopped above this by [`lean_max_memory_kib_for`]; the crash-loop breaker
/// ([`MAX_RESTARTS_PER_WINDOW`]) and the request deadline are this actor's own.
fn residue_restart_policy(config: &ActorConfig) -> LeanWorkerRestartPolicy {
    LeanWorkerRestartPolicy::default()
        .max_import_residue_bytes(config.import_residue_budget_bytes)
        .max_imports(WORKER_MAX_IMPORTS_BACKSTOP)
        .max_restarts_per_window(SUPERVISOR_RESTART_INTENSITY, RESTART_WINDOW)
}

fn worker_builder(config: &ActorConfig) -> LeanWorkerHostHandleBuilder {
    let restart_policy = residue_restart_policy(config);
    let module_cache_limits = module_cache_limits();
    LeanWorkerHostHandleBuilder::shims_only(&config.lake_root, std::iter::empty::<String>())
        .worker_child(LeanWorkerChild::for_toolchain(
            config.worker_path.clone(),
            config.lean_sysroot.clone(),
        ))
        .startup_timeout(Duration::from_secs(30))
        .request_timeout(Duration::from_millis(config.request_timeout_millis))
        .restart_policy(restart_policy)
        .session_pool_capacity(config.session_pool_capacity)
        .lean_max_memory_kib(config.lean_max_memory_kib)
        .module_cache_limits(module_cache_limits)
}

fn semantic_capability_builder(
    config: &ActorConfig,
    built: &lean_toolchain::LeanBuiltCapability,
) -> Result<LeanWorkerCapabilityBuilder> {
    // See `residue_restart_policy`: bounded by retained bytes, one Lean heap
    // ceiling.
    let restart_policy = residue_restart_policy(config);
    let builder = LeanWorkerCapabilityBuilder::from_built_capability(built, std::iter::empty::<String>())
        .map_err(map_worker_err)?
        .import_workspace_root(config.lake_root.clone())
        .worker_child(LeanWorkerChild::for_toolchain(
            config.worker_path.clone(),
            config.lean_sysroot.clone(),
        ))
        .startup_timeout(Duration::from_secs(30))
        .request_timeout(Duration::from_millis(config.request_timeout_millis))
        .restart_policy(restart_policy)
        .session_pool_capacity(config.session_pool_capacity)
        .lean_max_memory_kib(config.lean_max_memory_kib)
        .module_cache_limits(module_cache_limits())
        .json_command_export(lean_semantic_search_capability::DECLARATION_FEATURES_EXPORT)
        .json_command_export(lean_semantic_search_capability::PROOF_GOAL_FEATURES_EXPORT);
    Ok(builder)
}

/// Built semantic-search runtimes, one per `(toolchain label, Lean sysroot)`.
///
/// `lean_semantic_search_runtime::build_cached` is cached *on disk*, not in
/// process: every call re-materializes the source package, takes a filesystem
/// build lock, and runs a Lake build to conclude there is nothing to do.
/// Measured warm on this machine at **5.5 ms**, paid on the project's actor
/// thread — where it blocks the worker — on every semantic call. The result is
/// a pure function of the key, so it is computed once per process instead.
///
/// Process-wide rather than per project because that is the true scope: two
/// projects pinned to the same toolchain build the identical runtime into the
/// identical cache directory, and today they each pay for it.
///
/// The build itself runs *outside* the lock. Holding it would serialize every
/// project behind one Lake invocation; upstream's own filesystem lock already
/// makes a concurrent duplicate build safe, and the loser simply overwrites an
/// equal value.
static SEMANTIC_RUNTIMES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<(String, PathBuf), lean_semantic_search_runtime::SemanticSearchRuntime>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Memoized [`lean_semantic_search_runtime::build_cached`]; see
/// [`SEMANTIC_RUNTIMES`]. Failures are never cached — a build that failed
/// because a toolchain was mid-install must be retried, not remembered.
///
/// The error is a rendered message rather than a [`ServerError`] so the caller
/// keeps deciding what a failure means; it wraps this in the same structured
/// `worker_unavailable` it always did.
fn semantic_runtime(
    toolchain_label: &str,
    lean_sysroot: &Path,
) -> std::result::Result<lean_semantic_search_runtime::SemanticSearchRuntime, String> {
    let key = (toolchain_label.to_owned(), lean_sysroot.to_path_buf());
    // Bound the guard to a statement so the lock is released before the build
    // below, rather than living to the end of an `if let`.
    let cached = SEMANTIC_RUNTIMES.lock().get(&key).cloned();
    if let Some(runtime) = cached {
        return Ok(runtime);
    }
    let cache_root = semantic_runtime_cache_root().map_err(|err| err.to_string())?;
    let runtime = lean_semantic_search_runtime::build_cached(SemanticSearchRuntimeBuild {
        cache_root,
        toolchain_label: toolchain_label.to_owned(),
        lean_sysroot: lean_sysroot.to_path_buf(),
    })
    .map_err(|err| err.to_string())?;
    SEMANTIC_RUNTIMES.lock().insert(key, runtime.clone());
    Ok(runtime)
}

fn semantic_runtime_cache_root() -> Result<PathBuf> {
    let cache_dir =
        dirs::cache_dir().ok_or_else(|| ServerError::Internal("could not resolve user cache directory".to_owned()))?;
    Ok(cache_dir.join("lean-host-mcp").join("semantic-runtimes"))
}

/// Worker-side module snapshot cache limits.
///
/// The RSS guard is pinned to an unreachable ceiling, i.e. off. It is not a
/// knob because no finite value behaves sensibly: the worker clears the *whole*
/// snapshot cache at the top of every module-query batch whenever its RSS is at
/// or above the guard, and RSS counts the shared, clean, reclaimable `.olean`
/// pages Lean mmaps. Those alone put any Mathlib-scale worker past both our old
/// 2 GiB setting and the worker's own 3 GiB default, so the guard fired on
/// import-set size rather than on cache growth and wiped the cache on every
/// single query — making the snapshot cache unreachable in exactly the projects
/// that need it. Cache size is bounded by the worker's entry/TTL/byte limits,
/// which are the bounds that actually track the cache.
fn module_cache_limits() -> LeanWorkerModuleCacheLimits {
    LeanWorkerModuleCacheLimits::default().rss_guard_kib(0)
}

fn actor_thread_name(canonical_root: &Path) -> String {
    let basename = canonical_root.file_name().and_then(|s| s.to_str()).unwrap_or("project");
    format!("lean-host-mcp/project/{basename}")
}

fn worker_error_is_recoverable_death(err: &LeanWorkerError) -> bool {
    matches!(
        err,
        LeanWorkerError::ChildExited { .. } | LeanWorkerError::ChildPanicOrAbort { .. }
    )
}

fn worker_error_is_session_missing(err: &LeanWorkerError) -> bool {
    matches!(err, LeanWorkerError::Worker { code, .. } if code == "lean_rs.worker.session_missing")
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "only worker process death variants are restart causes; all other errors are classified elsewhere"
)]
fn worker_death_cause(err: &LeanWorkerError) -> RestartCause {
    match err {
        LeanWorkerError::ChildPanicOrAbort { .. } => RestartCause::ChildAbort,
        LeanWorkerError::ChildExited { .. } => RestartCause::ChildExit,
        _ => RestartCause::WorkerInternal,
    }
}

fn restart_reason_text(reason: &LeanWorkerRestartReason) -> String {
    match reason {
        LeanWorkerRestartReason::Explicit => RestartCause::Explicit.as_str().to_owned(),
        LeanWorkerRestartReason::MaxRequests { limit } => format!("max_requests limit={limit}"),
        LeanWorkerRestartReason::MaxImports { limit } => format!("max_imports limit={limit}"),
        LeanWorkerRestartReason::ImportResidue {
            residue_bytes,
            limit_bytes,
        } => format!(
            "import_residue residue_mib={} limit_mib={}",
            residue_bytes / (1024 * 1024),
            limit_bytes / (1024 * 1024)
        ),
        LeanWorkerRestartReason::RssCeiling {
            current_kib, limit_kib, ..
        } => {
            format!("rss_ceiling current_kib={current_kib} limit_kib={limit_kib}")
        }
        LeanWorkerRestartReason::RssHardLimit {
            operation,
            current_kib,
            limit_kib,
            ..
        } => {
            format!("rss_hard_limit operation={operation} current_kib={current_kib} limit_kib={limit_kib}")
        }
        LeanWorkerRestartReason::Idle { idle_for, limit } => {
            format!(
                "idle idle_for_millis={} limit_millis={}",
                millis_u64(*idle_for),
                millis_u64(*limit)
            )
        }
        LeanWorkerRestartReason::Cancelled { operation } => format!("cancelled operation={operation}"),
        // The child's stderr is drained once, at exit, and retained nowhere
        // else. Dropping it here would leave `child_abort operation=…` as the
        // whole record of a fatal child — enough to know something died, not
        // enough to tell a Lean out-of-memory panic from a segfault.
        LeanWorkerRestartReason::ChildAbort {
            operation,
            status,
            diagnostics,
        } => {
            let detail = diagnostics.trim();
            if detail.is_empty() {
                format!("child_abort operation={operation} status={status}")
            } else {
                format!("child_abort operation={operation} status={status}: {detail}")
            }
        }
        LeanWorkerRestartReason::RequestTimeout { operation, duration } => {
            format!(
                "timeout operation={operation} duration_millis={}",
                millis_u64(*duration)
            )
        }
        // `LeanWorkerRestartReason` is `#[non_exhaustive]`: a cycling policy
        // added upstream must surface as an unnamed cause rather than fail to
        // build here. `restart_cause_from_worker` maps the same case to
        // `WorkerInternal`, so the two agree.
        other => format!("worker_internal reason={}", other.stable_cause()),
    }
}

fn import_fingerprint(imports: &[String]) -> String {
    imports.join("\n")
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Resolve a knob through `env > file > default` and reject a zero result
/// whatever its source. `env` is the raw env-var string (parsed here); `file`
/// is the already-typed config-file value; `default` is the built-in constant.
fn parse_nonzero_u64(name: &str, env: Option<&str>, file: Option<u64>, default: u64) -> Result<u64> {
    let value = match env {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|e| ServerError::Internal(format!("{name}={raw:?} not a u64: {e}")))?,
        None => file.unwrap_or(default),
    };
    if value == 0 {
        return Err(ServerError::Internal(format!(
            "{name} resolved to 0, which is not allowed"
        )));
    }
    Ok(value)
}

fn parse_nonzero_usize(name: &str, env: Option<&str>, file: Option<usize>, default: usize) -> Result<usize> {
    let parsed = parse_nonzero_u64(name, env, file.map(|v| v as u64), default as u64)?;
    usize::try_from(parsed).map_err(|_| ServerError::Internal(format!("{name}={parsed} does not fit in usize")))
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

    #[test]
    fn runtime_config_parses_runtime_policy_without_env_reads() {
        // Distinct values so a field routed to the wrong knob is visible.
        let config = parse_runtime_config(
            RuntimeEnv {
                lean_max_memory_kib: Some("5".to_owned()),
                request_timeout_millis: Some("37".to_owned()),
                project_mailbox_capacity: Some("23".to_owned()),
                worker_restart_limit: Some("29".to_owned()),
                worker_restart_window_secs: Some("31".to_owned()),
                worker_import_residue_budget_mib: Some("41".to_owned()),
                worker_session_pool_capacity: Some("43".to_owned()),
            },
            &RuntimeFileConfig::default(),
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
    fn the_residue_budget_defaults_within_its_documented_clamp() {
        // Derived from system RAM, so the assertion is on the clamp rather than
        // a literal: the floor is what decides it on any ordinary machine, and
        // dropping below it would degenerate the policy into recycling before
        // every import.
        let budget = ProjectRuntimeConfig::default().import_residue_budget_bytes();
        assert!(
            (WORKER_IMPORT_RESIDUE_FLOOR_BYTES..=WORKER_IMPORT_RESIDUE_CEILING_BYTES).contains(&budget),
            "derived residue budget {budget} escaped its clamp"
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
        let env = RuntimeEnv {
            worker_session_pool_capacity: Some("2".to_owned()),
            ..RuntimeEnv::default()
        };
        let config = parse_runtime_config(env, &RuntimeFileConfig::default()).unwrap();
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
        let config = parse_runtime_config(RuntimeEnv::default(), &file).unwrap();
        assert_eq!(config.import_residue_budget_bytes(), 4_096 * 1024 * 1024);

        let env = RuntimeEnv {
            worker_import_residue_budget_mib: Some("2048".to_owned()),
            ..RuntimeEnv::default()
        };
        let config = parse_runtime_config(env, &file).unwrap();
        assert_eq!(config.import_residue_budget_bytes(), 2_048 * 1024 * 1024);
    }

    #[test]
    fn request_timeout_precedence_env_over_file_over_default() {
        let file = RuntimeFileConfig {
            request_timeout_millis: Some(45_000),
            ..RuntimeFileConfig::default()
        };
        // Env unset -> file value is used.
        let config = parse_runtime_config(RuntimeEnv::default(), &file).unwrap();
        assert_eq!(config.request_timeout_millis(), 45_000);
        // Env set -> env wins over the file.
        let env = RuntimeEnv {
            request_timeout_millis: Some("90000".to_owned()),
            ..RuntimeEnv::default()
        };
        let config = parse_runtime_config(env, &file).unwrap();
        assert_eq!(config.request_timeout_millis(), 90_000);
        // Neither -> built-in default (120 s).
        let config = parse_runtime_config(RuntimeEnv::default(), &RuntimeFileConfig::default()).unwrap();
        assert_eq!(config.request_timeout_millis(), REQUEST_TIMEOUT_MILLIS);
    }

    #[test]
    fn request_timeout_zero_is_rejected() {
        // A zero deadline would time every call out instantly; parse_nonzero_u64
        // must reject it.
        let err = parse_runtime_config(
            RuntimeEnv {
                request_timeout_millis: Some("0".to_owned()),
                ..RuntimeEnv::default()
            },
            &RuntimeFileConfig::default(),
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
        let config = parse_runtime_config(RuntimeEnv::default(), &file).unwrap();
        assert_eq!(config.lean_max_memory_kib(), 8_388_608);
        // Env set -> env wins over the file.
        let env = RuntimeEnv {
            lean_max_memory_kib: Some("6291456".to_owned()),
            ..RuntimeEnv::default()
        };
        let config = parse_runtime_config(env, &file).unwrap();
        assert_eq!(config.lean_max_memory_kib(), 6_291_456);
        // Neither -> built-in default.
        let config = parse_runtime_config(RuntimeEnv::default(), &RuntimeFileConfig::default()).unwrap();
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
            let config = parse_runtime_config(RuntimeEnv::default(), &file).unwrap();
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
        let small = import_residue_budget_for(8 * gib, 4);
        assert!(small < WORKER_IMPORT_RESIDUE_FLOOR_BYTES, "{small}");
        assert_eq!(small, 8 * gib - WORKER_IMPORT_HEADROOM_BYTES);
        // Absurd: never zero, because zero fails the nonzero MiB round-trip and
        // would take the server down rather than degrade it.
        assert_eq!(import_residue_budget_for(gib, 4), WORKER_IMPORT_RESIDUE_MIN_BYTES);
        assert!(import_residue_budget_for(0, 4) > 0);
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
        let err = parse_runtime_config(RuntimeEnv::default(), &zero).unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(
            message.contains("LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB"),
            "message: {message}"
        );
    }

    #[test]
    fn restart_stats_tally_total_and_per_cause() {
        let mut stats = RestartStats::default();
        stats.observe("rss_post_job");
        stats.observe("rss_post_job");
        stats.observe("child_abort");

        assert_eq!(stats.total, 3);
        assert_eq!(stats.by_cause.get("rss_post_job"), Some(&2));
        assert_eq!(stats.by_cause.get("child_abort"), Some(&1));
        assert_eq!(stats.by_cause.get("idle"), None);
    }

    #[test]
    fn semantic_capability_builder_omits_capability_import_module() {
        let tmp = tempfile::tempdir().unwrap();
        let capability_root = tmp.path().join("capability");
        let lib_dir = capability_root.join(".lake").join("build").join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        let dylib_name = if cfg!(target_os = "macos") {
            "libLeanSemanticSearch.dylib"
        } else {
            "libLeanSemanticSearch.so"
        };
        let dylib = lib_dir.join(dylib_name);
        std::fs::write(&dylib, "").unwrap();
        let built = lean_toolchain::LeanBuiltCapability::path(&dylib)
            .package("lean_semantic_search")
            .module("LeanSemanticSearch");
        let runtime = Arc::new(Mutex::new(RuntimeSnapshot {
            worker_generation: 1,
            last_restart: None,
            rss_kib: None,
            import_profile: None,
            profile_switch_count: 0,
            restarts_total: 0,
            restarts_by_cause: BTreeMap::new(),
            import_residue_bytes: None,
            import_residue_limit_bytes: None,
        }));
        let config = ActorConfig {
            lake_root: tmp.path().join("consumer"),
            manifest_hash: "sha256-test".to_owned(),
            toolchain_label: "leanprover/lean4:test".to_owned(),
            worker_path: tmp.path().join("worker"),
            lean_sysroot: tmp.path().join("lean"),
            session_id: "session-test".to_owned(),
            runtime,
            healthy: Arc::new(AtomicBool::new(true)),
            artifact_roots: Vec::new(),
            lean_max_memory_kib: ProjectRuntimeConfig::default().lean_max_memory_kib(),
            request_timeout_millis: REQUEST_TIMEOUT_MILLIS,
            mailbox_capacity: PROJECT_MAILBOX_CAPACITY,
            max_restarts_per_window: MAX_RESTARTS_PER_WINDOW,
            restart_window: RESTART_WINDOW,
            import_residue_budget_bytes: ProjectRuntimeConfig::default().import_residue_budget_bytes(),
            import_residue_soft_bytes: ProjectRuntimeConfig::default().import_residue_soft_bytes(),
            session_pool_capacity: WORKER_SESSION_POOL_CAPACITY,
            tokio_handle: None,
            toolchain_advisories: Vec::new(),
        };

        let builder = semantic_capability_builder(&config, &built).unwrap();
        let debug = format!("{builder:?}");

        assert!(debug.contains("imports: []"), "builder debug: {debug}");
        assert!(
            !debug.contains("LeanSemanticSearch.Capability"),
            "builder must not import the capability module: {debug}"
        );
        assert!(
            debug.contains("import_workspace_root: Some"),
            "builder should import sessions against the consumer workspace: {debug}"
        );
    }

    #[test]
    fn planned_restart_causes_do_not_consume_abnormal_restart_budget() {
        assert!(!RestartCause::RssPostJob.counts_toward_restart_limit());
        assert!(!RestartCause::MaxRequests.counts_toward_restart_limit());
        assert!(!RestartCause::MaxImports.counts_toward_restart_limit());
        assert!(!RestartCause::ImportResidue.counts_toward_restart_limit());
        assert!(!RestartCause::ImportResidueIdle.counts_toward_restart_limit());
        assert!(!RestartCause::Idle.counts_toward_restart_limit());

        assert!(RestartCause::ChildExit.counts_toward_restart_limit());
        assert!(RestartCause::ChildAbort.counts_toward_restart_limit());
        assert!(RestartCause::Timeout.counts_toward_restart_limit());
        assert!(RestartCause::Cancelled.counts_toward_restart_limit());
        assert!(RestartCause::SessionMissing.counts_toward_restart_limit());
        assert!(RestartCause::RssHardLimit.counts_toward_restart_limit());
    }

    #[test]
    fn worker_restart_reason_maps_to_stable_cause() {
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::MaxRequests { limit: 1 }).as_str(),
            "max_requests"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RssCeiling {
                current_kib: 2,
                limit_kib: 1,
                last_import_stats: None,
            })
            .as_str(),
            "rss_post_job"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RssHardLimit {
                operation: "test",
                current_kib: 2,
                limit_kib: 1,
                last_import_stats: None,
            })
            .as_str(),
            "rss_hard_limit_exceeded"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RequestTimeout {
                operation: "test",
                duration: Duration::from_millis(1),
            })
            .as_str(),
            "timeout"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::ImportResidue {
                residue_bytes: 2,
                limit_bytes: 1,
            })
            .as_str(),
            "import_residue"
        );
    }

    /// The reason string is the only place the two figures an operator needs —
    /// what was retained and what the limit was — reach the response, which is
    /// why they are not duplicated onto `RuntimeRestartEvent`.
    #[test]
    fn the_residue_reason_text_names_both_figures() {
        let text = restart_reason_text(&LeanWorkerRestartReason::ImportResidue {
            residue_bytes: 9 * 1024 * 1024 * 1024,
            limit_bytes: 12 * 1024 * 1024 * 1024,
        });
        assert_eq!(text, "import_residue residue_mib=9216 limit_mib=12288");
    }

    /// `LeanWorkerRestartReason` is `#[non_exhaustive]` and its `stable_cause`
    /// set is open, so a lean-rs upgrade can hand this build a cause it has
    /// never heard of. It lands on `worker_internal`, which must be treated as
    /// abnormal: `diagnosis::execution_taint` already counts it as disrupting,
    /// and a crash-loop breaker that excused the one cause it understands least
    /// would be exactly backwards. Every cause this build *does* know is
    /// classified precisely, so nothing routine reaches that bucket.
    #[test]
    fn an_unclassified_restart_is_abnormal_and_nothing_routine_reaches_it() {
        assert!(RestartCause::WorkerInternal.counts_toward_restart_limit());
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::ChildAbort {
                operation: "test",
                status: String::from("signal: 6 (SIGABRT)"),
                diagnostics: String::new(),
            })
            .as_str(),
            "child_abort",
            "a child abort is a named cause, not an unclassified one"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::Explicit).as_str(),
            "explicit"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::Idle {
                idle_for: Duration::from_millis(2),
                limit: Duration::from_millis(1),
            })
            .as_str(),
            "idle"
        );
    }
}
