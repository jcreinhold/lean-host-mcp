//! What a project-scope `find_references` actually costs, and where.
//!
//! `find_references` with `scope = project` answers from the on-disk `.ilean`
//! reference index rather than the worker, so its cost is entirely file I/O and
//! JSON parsing. Nothing measured it. The one number on record —
//! "~565 ms over a 500-module project" (`docs/ilean-reference-index.md:14-16`)
//! — is a single wall-clock sample from an `#[ignore]`d test, taken on a corpus
//! about a third the size of a real one, and it is the sole justification for
//! the reader being serial.
//!
//! A phase split over `~/Code/kan-proofs` (1431 `.ilean` files, 76 MB) puts the
//! cost decisively in one place: `stat` every file 0.010 s, read every byte
//! 0.066 s, **parse 1.32 s**. So the arms here are built to separate parse cost
//! from result construction, because that is the distinction any fix has to
//! move:
//!
//! - `whole_project` — the documented worst case: the most-referenced name in
//!   the corpus. Parse plus the full hit set.
//! - `whole_project_no_hits` — a name that cannot occur. Identical I/O and
//!   identical parsing, zero results, so the gap between this arm and the one
//!   above is what result construction costs and everything else is the scan
//!   floor.
//! - `narrowed_single_file` — the same query restricted to one file. Today this
//!   reads the whole project anyway and filters afterwards, so it should track
//!   `whole_project`; the point of the arm is that it stops doing so.
//!
//! Gated on `LEAN_HOST_MCP_BENCH_FIXTURE` (falling back to
//! `LEAN_HOST_MCP_TEST_FIXTURE`): a no-op when unset, so `cargo bench` still
//! runs in CI. The default name is `Fin`, which every Lean project reaches
//! through core; override per corpus with `LEAN_HOST_MCP_BENCH_ILEAN_NAME`.
//! `narrowed_single_file` needs a real source path and is skipped unless
//! `LEAN_HOST_MCP_BENCH_ILEAN_FILE` names one (relative to the project root) —
//! guessing one by inverting a module name would duplicate logic that belongs
//! to the reader.
//!
//! ```sh
//! LEAN_HOST_MCP_BENCH_FIXTURE=~/Code/kan-proofs \
//! LEAN_HOST_MCP_BENCH_ILEAN_FILE=KanProofs/CommutativeAlgebra/Cech/Localization.lean \
//!     cargo bench -p lean-host-mcp --bench ilean_reference_scan -- --save-baseline before
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::tools::position::{FindReferencesRequest, FindReferencesResult, ReferenceScope, find_references};
use lean_host_mcp::tools::{ToolConfig, ToolContext};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ResponseStatus};
use tokio::runtime::Runtime;

/// The most-referenced name in the kan-proofs corpus (9095 hits) and the
/// documented worst case. Reachable from core in any Lean project.
const DEFAULT_HOT_NAME: &str = "Fin";

/// Cannot be a Lean identifier, so it matches nothing while costing the same
/// walk, the same reads, and the same parse as the hot name.
const ABSENT_NAME: &str = "LeanHostMcp.Bench.NoSuchName.__absent";

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn request(name: &str, files: Vec<PathBuf>) -> FindReferencesRequest {
    FindReferencesRequest {
        name: name.to_owned(),
        scope: ReferenceScope::Project,
        file: None,
        files,
        limit: None,
        project: None,
    }
}

/// Runs one query and returns `(hits, modules_parsed)`.
///
/// Asserting the shape here keeps a fast failure from being timed as if it were
/// a scan: an `InvalidRequest` or a `runtime_unavailable` envelope would
/// otherwise measure an error path.
async fn scan(ctx: &ToolContext, name: &str, files: Vec<PathBuf>) -> (usize, usize) {
    let response = find_references(ctx, request(name, files))
        .await
        .expect("find_references");
    assert!(
        matches!(response.status, ResponseStatus::Ok),
        "bench arm must produce a real answer"
    );
    match response.result {
        Some(FindReferencesResult::Ok {
            references,
            files_scanned,
            ..
        }) => (references.len(), files_scanned),
        other => panic!("bench arm must return a project-scope answer, got {other:?}"),
    }
}

fn bench_ilean_reference_scan(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping ilean_reference_scan; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let hot_name = std::env::var("LEAN_HOST_MCP_BENCH_ILEAN_NAME").unwrap_or_else(|_| DEFAULT_HOT_NAME.to_owned());
    let narrowed_file = std::env::var("LEAN_HOST_MCP_BENCH_ILEAN_FILE").ok().map(PathBuf::from);

    let rt = Runtime::new().unwrap();
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
    });
    let ctx = ToolContext {
        broker,
        config: ToolConfig::default(),
    };

    // Report the corpus this run measured, so a number in a changelog can be
    // traced back to the shape that produced it. Project scope never spawns a
    // worker, so this is also the only setup the arms need.
    let (hot_hits, modules) = rt.block_on(scan(&ctx, &hot_name, Vec::new()));
    eprintln!("ilean_reference_scan: {modules} modules, {hot_hits} hits for `{hot_name}`");
    if hot_hits == 0 {
        eprintln!(
            "warning: `{hot_name}` has no hits here, so `whole_project` and \
             `whole_project_no_hits` measure the same thing; set \
             LEAN_HOST_MCP_BENCH_ILEAN_NAME to a name this corpus references"
        );
    }

    let mut group = c.benchmark_group("ilean_reference_scan");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(30));

    group.bench_function("whole_project", |b| {
        b.iter(|| {
            rt.block_on(scan(&ctx, &hot_name, Vec::new()));
        });
    });

    group.bench_function("whole_project_no_hits", |b| {
        b.iter(|| {
            rt.block_on(scan(&ctx, ABSENT_NAME, Vec::new()));
        });
    });

    match narrowed_file {
        Some(file) => {
            group.bench_function("narrowed_single_file", |b| {
                b.iter(|| {
                    rt.block_on(scan(&ctx, &hot_name, vec![file.clone()]));
                });
            });
        }
        None => eprintln!(
            "skipping narrowed_single_file; set LEAN_HOST_MCP_BENCH_ILEAN_FILE to a \
             source path relative to the project root"
        ),
    }

    group.finish();
}

criterion_group!(benches, bench_ilean_reference_scan);
criterion_main!(benches);
