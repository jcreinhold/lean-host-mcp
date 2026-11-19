//! Private guarded integration driver using the host's shims-only worker owner.
use lean_rs_worker_parent::{
    LeanWorkerChild, LeanWorkerHostHandleBuilder, LeanWorkerProcessTracker, LeanWorkerShutdownOutcome,
};
use lean_rs_worker_protocol::types::*;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
type E = Box<dyn std::error::Error>;
const ROOT: &str = env!("HOST_PLUGIN_TRIAL");
const SOURCE: &str = "import QualifiedPlugin\nexample : qualifiedPluginWitness = 73 := rfl\n";
fn main() -> Result<(), E> {
    let trial = Path::new(ROOT).canonicalize()?;
    let project = trial.join("project");
    let mode = std::env::args().nth(1).ok_or("missing mode")?;
    if mode == "census" {
        use lean_toolchain::native_guardian::{
            NativeOwnerObservation, NativeProcessIdentity, protected_native_reads_stopped,
        };
        use std::os::unix::fs::MetadataExt;
        let registry = trial.join("owners");
        for entry in fs::read_dir(&registry)? {
            let entry = entry?;
            if entry.file_name() == ".admission.lock" {
                continue;
            }
            let r: serde_json::Value = serde_json::from_slice(&fs::read(entry.path().join("owner.json"))?)?;
            let leader: NativeProcessIdentity = serde_json::from_value(r["process"].clone())?;
            let guardian: NativeProcessIdentity = serde_json::from_value(r["guardian"].clone())?;
            assert_eq!(leader.observe()?, NativeOwnerObservation::Absent);
            assert!(!guardian.is_live()?);
            for domain in r["protected_domains"].as_array().ok_or("domains")? {
                let path = Path::new(domain["lock_path"].as_str().ok_or("lock")?);
                let file = fs::File::open(path)?;
                file.try_lock()?;
                let meta = file.metadata()?;
                assert_eq!(meta.dev(), domain["device"].as_u64().unwrap());
                assert_eq!(meta.ino(), domain["inode"].as_u64().unwrap());
                assert!(protected_native_reads_stopped(
                    &registry,
                    path,
                    (meta.dev(), meta.ino())
                )?);
            }
            println!(
                "generation {:?}: owned group absent, exact guardian stopped, {:?} domains checked under exclusive custody",
                r["generation_id"],
                r["protected_domains"].as_array().unwrap().len()
            );
        }
        return Ok(());
    }
    let lib = project.join(".lake/build/lib/lean");
    let compiled = lib.join("QualifiedPlugin.olean");
    let saved = project.join("original-private-module");
    if mode == "replace" || mode == "restore" {
        if std::env::current_dir()?.canonicalize()? != project {
            return Err("writer cwd mismatch".into());
        }
        if mode == "replace" {
            if saved.exists() {
                return Err("replacement already present".into());
            }
            fs::rename(&compiled, &saved)?;
            fs::copy(trial.join("alternate/QualifiedPlugin.olean"), &compiled)?;
        } else {
            fs::rename(&saved, &compiled)?;
        }
        return Ok(());
    }
    let helper = PathBuf::from(std::env::var_os("LEAN_HOST_MCP_WRITER_EXECUTABLE").ok_or("helper")?);
    if mode == "writer-replace" || mode == "writer-restore" {
        let mut writer = lean_toolchain::shared_writer::SharedWriter::open(&helper, &project, Duration::from_secs(8))?;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg(mode.strip_prefix("writer-").unwrap()).current_dir(&project);
        let result = writer.run(&cmd, Duration::from_secs(8))?;
        if !result.status.success() {
            return Err(format!("private writer failed {result:?}").into());
        }
        println!("managed {mode} succeeded");
        return Ok(());
    }
    let d: serde_json::Value = serde_json::from_slice(&fs::read(trial.join("worker/deployment.json"))?)?;
    let prepared = serde_json::from_value(
        serde_json::json!({"protocol":1,"shim_manifest_path":d["shim_manifest"],"shim_manifest_json":d["shim_json"],"artifact_roots":d["shim_roots"],"shim_search_paths":d["shim_paths"]}),
    )?;
    let marker = trial.join("worker/marker");
    let log = trial.join("worker/native-events.log");
    let start = fs::read_to_string(&log).unwrap_or_default();
    let tracker = LeanWorkerProcessTracker::new();
    let child = LeanWorkerChild::for_toolchain(
        PathBuf::from(std::env::var_os("HOST_PLUGIN_WORKER").ok_or("missing fixture worker")?),
        PathBuf::from(d["sysroot"].as_str().unwrap()),
    )
    .checked_native_loader_environment()
    .native_guardian(helper.clone())
    .shared_writer_executable(helper)
    .lifetime_directory(PathBuf::from(
        std::env::var_os("LEAN_RS_WORKER_OWNERS_DIRECTORY").ok_or("registry")?,
    ))
    .prepared_host_artifacts(prepared)
    .shared_artifact_reads(true);
    let names = vec!["QualifiedPlugin".to_string()];
    let builder = LeanWorkerHostHandleBuilder::shims_only(&project, names.clone())
        .worker_child(child)
        .session_pool_capacity(2)
        .startup_timeout(Duration::from_secs(35))
        .request_timeout(Duration::from_secs(65))
        .process_tracker(tracker.clone());
    let opened = builder.open();
    if mode == "override" {
        assert!(std::env::var_os("DYLD_LIBRARY_PATH").is_some());
        let error = opened.err().ok_or("inherited loader override accepted")?;
        assert!(error.to_string().contains("DYLD_LIBRARY_PATH"), "{error}");
        assert_eq!(fs::read_to_string(&log).unwrap_or_default(), start, "worker entered");
    } else if mode == "refused" {
        let e = opened.err().ok_or("replacement unexpectedly authorized")?;
        let events = fs::read_to_string(log)?;
        let delta = events.strip_prefix(&start).ok_or("audit prefix")?;
        assert!(
            delta.contains("refused wrong file identity")
                && !delta.contains("grant-")
                && !delta.contains("authorized-plugin"),
            "{e}: {delta}"
        );
        println!("replacement refused before import or source: {e}");
    } else {
        if mode != "positive" {
            return Err("unknown mode".into());
        }
        let mut h = opened?;
        assert_eq!(fs::read(&marker)?, b"initialized");
        let opts = LeanWorkerElabOptions {
            namespace_context: String::new(),
            file_label: "HostTrial.lean".into(),
            heartbeat_limit: 100000,
            diagnostic_byte_limit: 8192,
            lean_options: Default::default(),
        };
        let generation = h.lifecycle_snapshot().worker_generation;
        let r = h.process_module_query_with_imports(
            names.clone(),
            SOURCE,
            &LeanWorkerModuleQuery::Diagnostics,
            &opts,
            None,
            None,
        )?;
        assert!(
            matches!(r,LeanWorkerModuleQueryOutcome::Ok{result: LeanWorkerModuleQueryResult::Diagnostics(ref f),..} if f.diagnostics.is_empty() && !f.truncated)
        );
        assert!(
            fs::read_to_string(&log)?
                .strip_prefix(&start)
                .ok_or("audit prefix")?
                .contains("grant-single")
        );
        let report = h.shutdown()?;
        assert_eq!(report.outcome, LeanWorkerShutdownOutcome::Graceful);
        println!("generation {generation}, shutdown {:?}", report.outcome);
    }
    let until = Instant::now() + Duration::from_secs(5);
    while tracker.active_processes() != 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(tracker.active_processes(), 0, "owned tracker still active");
    println!("active tracked processes 0");
    Ok(())
}
