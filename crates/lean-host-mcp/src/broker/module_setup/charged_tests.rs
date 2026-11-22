//! Run the actual broker in a child test process so its registry is private from birth.
use super::*;
use crate::{broker::BrokerConfig, project::ProjectRuntimeConfig};
use lean_toolchain::shared_writer::{WriterDisposition, WriterFailure};
use serde_json::json;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

fn request(project: &Path, registry: &Path, scratch: &Path, budget: u64) -> io::Result<String> {
    let roots = [
        ("project", project),
        ("registry", registry),
        ("scratch_parent", scratch),
    ]
    .map(|(role, path)| {
        let meta = path.metadata()?;
        io::Result::Ok(json!({"role":role, "path":path, "device":meta.dev(), "inode":meta.ino()}))
    })
    .into_iter()
    .collect::<io::Result<Vec<_>>>()?;
    Ok(json!({"version":1,"budget_bytes":budget,"floor_bytes":1,"roots":roots}).to_string())
}

#[tokio::test]
async fn charged_setup_requires_positive_budget_and_bound_capability() -> io::Result<()> {
    let root = Path::new("/project");
    let lock = Path::new("/lock");
    let writer = ChargedWriter {
        executable: PathBuf::from("/unselected"),
        request: json!({"version":1, "budget_bytes":0}).to_string(),
    };
    let error = writer
        .qualify(root, Duration::from_secs(1))
        .await
        .err()
        .ok_or_else(|| invalid("zero budget was accepted"))?;
    assert!(error.to_string().contains("full-turn storage budget"));
    let descriptor = |project: &str, lock: &str, caps: &[&str]| {
        json!({"protocol":1,"project_root":project,"lock_path":lock,"capabilities":caps}).to_string()
    };
    let caps = ["lake_module_setup_capture_v1"];
    for (project, domain, capabilities) in [
        ("/other", "/lock", caps.as_slice()),
        ("/project", "/other", caps.as_slice()),
        ("/project", "/lock", ["command_only_storage_v1"].as_slice()),
    ] {
        assert!(check_capability(descriptor(project, domain, capabilities).as_bytes(), root, lock).is_err());
    }
    check_capability(descriptor("/project", "/lock", &caps).as_bytes(), root, lock)
}

#[tokio::test]
#[ignore = "requires an explicitly selected SDK writer and the repository-pinned Elan release"]
async fn charged_setup_against_peer() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let fixture_name = "LEAN_HOST_MODULE_SETUP_TEST_ROOT";
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let target = workspace.join("target");
    let writer = PathBuf::from(std::env::var("LEAN_HOST_MODULE_SETUP_TEST_WRITER")?).canonicalize()?;
    let Some(fixture) = std::env::var_os(fixture_name) else {
        let fixture = tempfile::Builder::new().prefix("charged-setup-").tempdir_in(&target)?;
        let registry = fixture.path().join("registry");
        fs::create_dir(&registry)?;
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o700))?;
        let status = Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "broker::module_setup::charged_tests::charged_setup_against_peer",
            ])
            .env(fixture_name, fixture.path())
            .env("LEAN_RS_WORKER_OWNERS_DIRECTORY", registry)
            .env("TMPDIR", fixture.path())
            .status()?;
        assert!(status.success());
        return Ok(());
    };
    let base = PathBuf::from(fixture).canonicalize()?;
    assert_eq!(base.parent(), Some(target.as_path()));
    let registry = base.join("registry");
    assert_eq!(lean_toolchain::native_guardian::registry_directory()?, registry);
    let scratch = base.join("scratch");
    let project = base.join("project");
    let peer = base.join("peer");
    for dir in [&scratch, &project, &peer] {
        fs::create_dir(dir)?;
    }
    let pin = fs::read_to_string(workspace.join("fixtures/lean/lean-toolchain"))?;
    fs::write(project.join("lean-toolchain"), pin)?;
    fs::write(
        project.join("lake-manifest.json"),
        br#"{"version":"1.1.0","packagesDir":".lake/packages","packages":[],"name":"demo","lakeDir":".lake"}"#,
    )?;
    let value = "x".repeat(72 * 1024);
    fs::write(
        project.join("lakefile.toml"),
        format!("name = \"demo\"\n[[lean_lib]]\nname = \"Demo\"\nleanOptions = {{ fixture = \"{value}\" }}\n"),
    )?;
    let source = "prelude\n";
    let file = project.join("Demo.lean");
    fs::write(&file, source)?;
    let file = file.to_str().ok_or_else(|| invalid("non-UTF-8 fixture"))?;
    let broker = ProjectBroker::new_with_runtime_config(
        BrokerConfig {
            config_default: None,
            env_default: None,
            cwd: project.clone(),
            max_projects: std::num::NonZeroUsize::MIN,
            max_workers_per_project: std::num::NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
        },
        ProjectRuntimeConfig::default().with_operator_primary(Some(&project))?,
    );
    let hint = ProjectHint::Explicit(project.clone());
    let stat = rustix::fs::statvfs(&base)?;
    let free = stat
        .f_bavail
        .checked_mul(stat.f_frsize)
        .ok_or_else(|| invalid("fixture size overflow"))?;
    let budget = free - free / 4;
    let selected = || -> io::Result<ChargedWriter> {
        Ok(ChargedWriter {
            executable: writer.clone(),
            request: request(&project, &registry, &scratch, budget)?,
        })
    };
    let timeout = Duration::from_secs(5);
    let holder = SharedWriter::open_with_storage_request(
        &writer,
        &peer,
        &[],
        &request(&peer, &registry, &scratch, budget)?,
        timeout,
        &|| false,
    )?;
    let held_scratch = holder.asset_dir().to_owned();
    let before = scratch.metadata()?.modified()?;
    let error = broker
        .prepare_file_setup(&hint, file, source, Some(selected()?))
        .await
        .err()
        .ok_or_else(|| invalid("unexpected setup admission"))?;
    assert!(
        matches!(&error, ServerError::Io(io) if WriterFailure::from_io(io).is_some_and(|f| f.disposition == WriterDisposition::NotStarted)),
        "{error}"
    );
    assert_eq!(scratch.metadata()?.modified()?, before);
    assert!(held_scratch.is_dir());
    assert!(!project.join(".lake").exists());
    let charges = registry.join(".writer-storage-v1");
    assert_eq!(fs::read_dir(&charges)?.count(), 1);
    holder.close(timeout)?;
    assert!(!held_scratch.exists());
    let setup = broker
        .prepare_file_setup(&hint, file, source, Some(selected()?))
        .await?;
    assert!((72 * 1024..80 * 1024).contains(&setup.bytes.len()));
    assert_eq!(
        setup.options.get("fixture"),
        Some(&lean_toolchain::LeanOptionValue::String(value))
    );
    assert_eq!(fs::read_dir(&scratch)?.count(), 0);
    assert!(
        !charges.try_exists()?,
        "the final settlement removes the empty charge directory"
    );
    Ok(())
}
