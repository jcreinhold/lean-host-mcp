//! Private long-lived-controller fixtures for utility admission and cleanup.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::*;

const PAYLOAD: &str = r#"#!/usr/bin/env python3
import os,subprocess,sys,time
from pathlib import Path
root=Path(__file__).parent; assets=Path(sys.argv[1]); name=sys.argv[2]
(root/('started-'+name)).write_text(str(os.getpid()))
if os.environ.get('UTILITY_TEST_LEAF'):
 leaf = "import os,sys,time\nfrom pathlib import Path\nroot=Path(sys.argv[1])\nfor fd in range(0,4096):\n try: os.close(fd)\n except OSError: pass\n(root/'started-leaf').write_text(str(os.getpid()))\nwhile True:\n (root/'leaf-heartbeat').write_text(str(time.monotonic_ns()))\n time.sleep(.01)\n"
 subprocess.Popen([sys.executable,'-c',leaf,str(root)],close_fds=True)
 while not (root/'started-leaf').exists(): time.sleep(.01)
if os.environ.get('UTILITY_TEST_INTERLOCK'):
 lock=root/'active-utility'
 try: lock.mkdir()
 except FileExistsError: sys.exit(72)
 time.sleep(.08)
 lock.rmdir()
while not (root/('go-'+name)).exists():
 assert (assets/'input').read_text() == name
 time.sleep(.01)
print((assets/'input').read_text())
"#;

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    script: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("Project");
        fs::create_dir_all(project.join(".lake")).unwrap();
        fs::write(project.join("lake-manifest.json"), "{\"packages\":[]}").unwrap();
        let script = root.join("payload.py");
        fs::write(&script, PAYLOAD).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _directory: directory,
            root,
            project,
            script,
        }
    }
    fn contract(&self, helper: PathBuf) -> crate::shared_writer::WriterContract {
        crate::shared_writer::WriterContract {
            protocol: crate::shared_writer::PROTOCOL,
            executable: helper,
            ownership_registry: self.root.join("owners"),
        }
    }
    fn program(&self, assets: &Path, name: &str, interlock: bool) -> io::Result<NativeReaderProgram> {
        fs::write(assets.join("input"), name)?;
        Ok(NativeReaderProgram {
            command: vec![
                self.script.clone().into_os_string(),
                assets.as_os_str().to_os_string(),
                name.into(),
            ],
            environment: if interlock {
                vec![("UTILITY_TEST_INTERLOCK".into(), "1".into())]
            } else {
                vec![]
            },
            cwd: self.project.clone(),
            artifact_roots: vec![],
        })
    }
    fn assert_exclusive_available(&self) {
        let path = lean_toolchain::shared_writer::project_lock_path(&self.project).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert!(lock.try_lock().is_ok());
    }
}

fn helper() -> PathBuf {
    PathBuf::from(std::env::var_os("LEAN_HOST_MCP_NATIVE_READER_TEST_HELPER").expect("opt-in actual H binary"))
        .canonicalize()
        .unwrap()
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now().checked_add(Duration::from_secs(5)).unwrap();
    while !predicate() {
        assert!(Instant::now() < deadline, "bounded utility fixture transition");
        thread::sleep(POLL);
    }
}

#[test]
fn preparer_failure_is_unstarted_and_drops_private_inputs_without_a_child() {
    let fixture = Fixture::new();
    let input = Arc::new(Mutex::new(None::<PathBuf>));
    let captured = Arc::clone(&input);
    let result = run_program_with_contract(
        &fixture.contract(PathBuf::from("/usr/bin/true")),
        &fixture.project,
        Duration::from_secs(1),
        1,
        &|| false,
        None,
        move |assets| {
            *captured.lock().unwrap() = Some(assets.to_path_buf());
            fs::write(assets.join("private"), "input")?;
            Err(io::Error::other("private source preparation failed"))
        },
    )
    .unwrap();
    assert_eq!(result.state, "not_started");
    assert_eq!(result.target_started, Some(false));
    assert!(result.scope_closed);
    assert!(!input.lock().unwrap().as_ref().unwrap().exists());
    assert!(!fixture.root.join("started-utility").exists());
}

#[test]
fn cancelled_before_admission_never_prepares_or_spawns() {
    let fixture = Fixture::new();
    let prepared = AtomicBool::new(false);
    let result = run_program_with_contract(
        &fixture.contract(PathBuf::from("/usr/bin/true")),
        &fixture.project,
        Duration::from_secs(1),
        1,
        &|| true,
        None,
        |_| {
            prepared.store(true, Ordering::SeqCst);
            Err(io::Error::other("preparer must remain unreachable"))
        },
    )
    .unwrap();
    assert_eq!(result.state, "not_started");
    assert_eq!(result.target_started, Some(false));
    assert!(result.scope_closed);
    assert!(!prepared.load(Ordering::SeqCst));
    assert!(!fixture.root.join("owners").exists());
}

#[test]
#[ignore = "requires LEAN_HOST_MCP_NATIVE_READER_TEST_HELPER actual parent binary"]
fn concurrent_utilities_wait_before_spawn_while_resident_capacity_is_full() {
    for limit in [1, 3] {
        let fixture = Arc::new(Fixture::new());
        let helper = helper();
        let contract = fixture.contract(helper.clone());
        let resident_assets = fixture.root.join("resident-inputs");
        fs::create_dir(&resident_assets).unwrap();
        fs::write(resident_assets.join("input"), "resident").unwrap();
        let mut warm = Command::new(&helper)
            .arg("native-reader")
            .arg("--project-root")
            .arg(&fixture.project)
            .arg("--capacity-limit")
            .arg(limit.to_string())
            .arg("--timeout-millis")
            .arg("15000")
            .arg("--")
            .arg(&fixture.script)
            .arg(&resident_assets)
            .arg("resident")
            .env("LEAN_RS_WORKER_OWNERS_DIRECTORY", &contract.ownership_registry)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for(|| fixture.root.join("started-resident").exists());
        let threads = (0..3)
            .map(|index| {
                let fixture = Arc::clone(&fixture);
                let contract = contract.clone();
                let name = format!("utility-{index}");
                fs::write(fixture.root.join(format!("go-{name}")), "go").unwrap();
                thread::spawn(move || {
                    run_program_with_contract(
                        &contract,
                        &fixture.project,
                        Duration::from_secs(5),
                        limit,
                        &|| false,
                        None,
                        |assets| fixture.program(assets, &name, true),
                    )
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            let result = thread.join().unwrap();
            assert_eq!(result.state, "completed", "{result:?}");
            assert_eq!(result.exit_code, Some(0));
            assert!(result.scope_closed);
        }
        fs::write(fixture.root.join("go-resident"), "go").unwrap();
        assert!(warm.wait().unwrap().success());
        fixture.assert_exclusive_available();
    }
}

#[test]
#[ignore = "requires LEAN_HOST_MCP_NATIVE_READER_TEST_HELPER actual parent binary"]
fn late_cleanup_after_unknown_reaps_and_reclaims_inputs_without_controller_exit() {
    let fixture = Arc::new(Fixture::new());
    let contract = fixture.contract(helper());
    let inputs = Arc::new(Mutex::new(None::<PathBuf>));
    let captured_inputs = Arc::clone(&inputs);
    let dropped = Arc::new(AtomicBool::new(false));
    let premature = Arc::new(AtomicBool::new(false));
    let record = Arc::new(Mutex::new(None::<PathBuf>));
    struct Probe {
        dropped: Arc<AtomicBool>,
        premature: Arc<AtomicBool>,
        record: Arc<Mutex<Option<PathBuf>>>,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            let closed = (|| -> io::Result<bool> {
                let path = self
                    .record
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| io::Error::other("owned record missing"))?;
                let owner: serde_json::Value = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
                let process: NativeProcessIdentity =
                    serde_json::from_value(owner["process"].clone()).map_err(io::Error::other)?;
                let guardian: NativeProcessIdentity =
                    serde_json::from_value(owner["guardian"].clone()).map_err(io::Error::other)?;
                Ok(!lean_toolchain::process_group::group_present(process.process_group)? && !guardian.is_live()?)
            })();
            self.premature.store(!matches!(closed, Ok(true)), Ordering::SeqCst);
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    let keepalive: Box<dyn Send> = Box::new(Probe {
        dropped: Arc::clone(&dropped),
        premature: Arc::clone(&premature),
        record: Arc::clone(&record),
    });
    let controller_pid = std::process::id();
    let result = run_program_with_contract(
        &contract,
        &fixture.project,
        Duration::from_secs(15),
        1,
        &|| {
            if fixture.root.join("started-utility").exists() {
                let path = inputs.lock().unwrap().clone().unwrap();
                assert!(path.join("input").exists());
                assert!(!dropped.load(Ordering::SeqCst));
                *record.lock().unwrap() = Some(path.parent().unwrap().join("owner.json"));
                panic!("injected owner failure after native payload admission");
            }
            false
        },
        Some(keepalive),
        |assets| {
            *captured_inputs.lock().unwrap() = Some(assets.to_path_buf());
            fixture.program(assets, "utility", false)
        },
    )
    .unwrap();
    assert_eq!(std::process::id(), controller_pid);
    assert_eq!(result.state, "unknown");
    assert!(!result.scope_closed);
    assert!(result.exit_status.is_none());
    let input_path = inputs.lock().unwrap().clone().unwrap();
    assert_eq!(input_path.file_name().unwrap(), "inputs");
    assert!(input_path.starts_with(&contract.ownership_registry));
    let record = input_path.parent().unwrap().join("owner.json");
    let owner: serde_json::Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
    assert_eq!(owner["parent"]["pid"], controller_pid);
    let process: NativeProcessIdentity = serde_json::from_value(owner["process"].clone()).unwrap();
    let guardian: NativeProcessIdentity = serde_json::from_value(owner["guardian"].clone()).unwrap();
    wait_for(|| {
        !input_path.exists()
            && !lean_toolchain::process_group::group_present(process.process_group).unwrap()
            && !guardian.is_live().unwrap()
    });
    wait_for(|| dropped.load(Ordering::SeqCst));
    assert!(
        !premature.load(Ordering::SeqCst),
        "admission/source custody dropped before actual native scope closure"
    );
    fixture.assert_exclusive_available();
    fs::write(fixture.root.join("go-next"), "go").unwrap();
    let result = run_program_with_contract(
        &contract,
        &fixture.project,
        Duration::from_secs(5),
        1,
        &|| false,
        None,
        |assets| fixture.program(assets, "next", false),
    )
    .unwrap();
    assert_eq!(result.state, "completed");
    assert!(result.scope_closed);
}

#[test]
#[ignore = "internal real-guardian fixture; only the owned dispatcher sets its private environment"]
#[allow(
    clippy::exit,
    reason = "isolated guardian fault fixture terminates only its own process"
)]
fn guardian_self_exit_fixture() {
    let Some(root) = std::env::var_os("UTILITY_GUARDIAN_SELF_EXIT_ROOT").map(PathBuf::from) else {
        return;
    };
    thread::spawn(move || {
        let deadline = Instant::now().checked_add(Duration::from_secs(8)).unwrap();
        while !root.join("started-leaf").exists() {
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(POLL);
        }
        fs::remove_file(root.join("guardian-self-exit-once")).unwrap();
        // Fault injection can only terminate this actual fixture process.
        // No persisted or sampled PID is ever used as signal authority.
        std::process::exit(88);
    });
    lean_toolchain::native_guardian::run_guardian_cli().unwrap();
}

#[test]
#[ignore = "requires LEAN_HOST_MCP_NATIVE_READER_TEST_HELPER actual parent binary"]
fn guardian_self_exit_with_live_controller_reaps_group_and_recovers_capacity() {
    let fixture = Fixture::new();
    let helper = helper();
    let dispatcher = fixture.root.join("dispatch.py");
    let test_binary = std::env::current_exe().unwrap().canonicalize().unwrap();
    fs::write(
        &dispatcher,
        format!(
            "#!/usr/bin/env python3\nimport os,sys\nfrom pathlib import Path\nroot=Path(__file__).parent\nif sys.argv[1:] == ['native-guardian'] and (root/'guardian-self-exit-once').exists():\n os.environ['UTILITY_GUARDIAN_SELF_EXIT_ROOT']=str(root)\n os.execv({test_binary:?},[{test_binary:?},'--exact','native_reader::program_tests::guardian_self_exit_fixture','--ignored','--nocapture'])\nos.execv({helper:?},[{helper:?}]+sys.argv[1:])\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&dispatcher, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(fixture.root.join("guardian-self-exit-once"), "once").unwrap();
    let contract = fixture.contract(dispatcher);
    let started = Instant::now();
    let result = run_program_with_contract(
        &contract,
        &fixture.project,
        Duration::from_secs(15),
        1,
        &|| false,
        None,
        |assets| {
            let mut program = fixture.program(assets, "utility", false)?;
            program.environment.push(("UTILITY_TEST_LEAF".into(), "1".into()));
            Ok(program)
        },
    )
    .unwrap();
    assert_eq!(result.state, "stopped", "{result:?}");
    assert!(result.scope_closed, "{result:?}");
    assert!(result.exit_status.is_none());
    assert!(
        result
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("native guardian exited")),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "guardian loss must precede the 15s deadline"
    );
    assert!(!fixture.root.join("guardian-self-exit-once").exists());
    fixture.assert_exclusive_available();
    fs::write(fixture.root.join("go-next"), "go").unwrap();
    let next = run_program_with_contract(
        &contract,
        &fixture.project,
        Duration::from_secs(5),
        1,
        &|| false,
        None,
        |assets| fixture.program(assets, "next", false),
    )
    .unwrap();
    assert_eq!(next.state, "completed", "{next:?}");
    assert_eq!(next.exit_code, Some(0));
}

#[test]
fn explicit_domain_input_limit_preserves_deduplication_and_complete_union_bound() {
    let fixture = Fixture::new();
    let mut args = NativeReaderArgs {
        project_root: fixture.project.clone(),
        artifact_root: vec![fixture.project.clone(); lean_toolchain::native_guardian::MAX_READ_DOMAINS],
        timeout_millis: 1_000,
        capacity_limit: 1,
        stdin: false,
        cancel_fd: None,
        stdout_result_frame: false,
        diagnostic_preview: false,
        max_output_bytes: OUTPUT_LIMIT,
        command: vec![fixture.script.into_os_string()],
    };
    args.validate().expect("32 canonical explicit inputs are allowed");
    assert_eq!(
        args.roots().expect("deduplicate physical domains"),
        vec![fixture.project.clone()]
    );
    args.artifact_root.push(fixture.project);
    assert!(
        args.validate().is_err(),
        "33 explicit inputs exceed the bounded input list"
    );
}

#[test]
fn failed_bootstrap_preserves_primary_error_and_independent_closure() {
    let source = io::Error::other("HELLO identity mismatch");
    let before = not_started(&source);
    assert_eq!(before.state, "not_started");
    assert_eq!(before.target_started, Some(false));
    assert!(before.detail.as_deref().unwrap().contains("HELLO identity mismatch"));

    for (closed, state) in [(false, "unknown"), (true, "stopped")] {
        let after = startup_failure(&source, closed);
        assert_eq!(after.scope_closed, closed);
        assert_eq!(after.state, state);
        assert_eq!(after.target_started, Some(false));
        assert_eq!(
            after.detail.as_deref(),
            Some("native startup refused before payload ACK: HELLO identity mismatch")
        );
    }
}
