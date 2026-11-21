//! File setup from the selected Lake. Keep Lake's original frame, including
//! import visibility, plugin entry points and compiler options.
use super::{ProjectBroker, ProjectHint, Result, ServerError};
use crate::cancellation::CallCancellation;
use lean_toolchain::shared_writer::SharedWriter;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModuleSetup {
    name: String,
    #[serde(rename = "package")]
    _package: Option<String>,
    #[serde(rename = "isModule")]
    _is_module: bool,
    #[serde(rename = "imports")]
    _imports: Option<Vec<serde_json::Value>>,
    import_arts: BTreeMap<String, Vec<Vec<PathBuf>>>,
    plugins: Vec<Plugin>,
    dynlibs: Vec<PathBuf>,
    #[serde(rename = "options")]
    _options: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Plugin {
    Path(PathBuf),
    Named {
        path: PathBuf,
        #[serde(rename = "initFn")]
        init_fn: Option<String>,
    },
}

fn stage(bytes: &[u8], writer: &SharedWriter, sysroot: &Path) -> io::Result<()> {
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("Lake module setup exceeds capture limit"));
    }
    let setup: ModuleSetup = serde_json::from_slice(bytes)?;
    if setup.name.is_empty() {
        return Err(invalid("missing Lake module name"));
    }
    // These fields pass through byte-for-byte. Only Lake/Lean interprets them.
    let release_lib = sysroot.join("lib");
    let validate = |p: &Path| -> io::Result<()> {
        if !p.is_absolute()
            || p.canonicalize()? != p
            || !p.is_file()
            || !(p.starts_with(&release_lib)
                || writer
                    .write_roots()
                    .iter()
                    .any(|r| p.starts_with(r.join(".lake/build"))))
        {
            return Err(invalid(
                "Lake setup artifact outside held project outputs or selected release",
            ));
        }
        Ok(())
    };
    for parts in setup.import_arts.values() {
        if parts.is_empty()
            || parts.len() > 2
            || parts.first().is_none_or(|oleans| oleans.is_empty() || oleans.len() > 3)
            || parts.get(1).is_some_and(|ir| ir.len() > 2)
        {
            return Err(invalid("unsupported Lake import artifact layout"));
        }
        for path in parts.iter().flatten() {
            validate(path)?;
        }
    }
    for path in &setup.dynlibs {
        validate(path)?;
    }
    for plugin in &setup.plugins {
        match plugin {
            Plugin::Path(p) => validate(p)?,
            Plugin::Named { path, init_fn } => {
                if init_fn.as_ref().is_some_and(String::is_empty) {
                    return Err(invalid("empty plugin initializer"));
                }
                validate(path)?;
            }
        }
    }
    fs::write(writer.asset_dir().join("module.setup.json"), bytes)
}

impl ProjectBroker {
    pub(crate) async fn prepare_file_setup(&self, hint: &ProjectHint, file: &str, source: &str) -> Result<()> {
        let root = self.native_root(hint)?;
        let activity = self.preparation_activity(&root).await?;
        let timeout = Duration::from_millis(self.request_timeout_millis());
        let file = PathBuf::from(file);
        let source = source.to_owned();
        let cancel = CallCancellation::current();
        let mut on_drop = cancel.cancel_on_drop();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            let _activity = activity;
            let helper = crate::shared_writer::contract()?.executable;
            let started = Instant::now();
            let mut writer = SharedWriter::open_cancellable(&helper, &root, timeout, &|| cancel.is_cancelled())?;
            if !file.starts_with(&root)
                || file.canonicalize()? != file
                || file.extension().is_none_or(|s| s != "lean")
                || fs::read(&file)? != source.as_bytes()
            {
                return Err(ServerError::BadProject(
                    "current source differs from selected verification file".into(),
                ));
            }
            let id = crate::toolchain::ToolchainId::from_lake_root(&root)
                .map_err(|e| ServerError::BadProject(e.to_string()))?;
            let sysroot = id
                .elan_dir()
                .map_err(|e| ServerError::BadProject(e.to_string()))?
                .canonicalize()?;
            let mut command = Command::new(sysroot.join("bin/lake"));
            command
                .current_dir(&root)
                .args(["--no-build", "--no-cache", "setup-file"])
                .arg(&file)
                .env("LAKE_CACHE_DIR", writer.asset_dir())
                .env("LAKE_ARTIFACT_CACHE", "false");
            let result = writer.run_cancellable(&command, timeout.saturating_sub(started.elapsed()), &|| {
                cancel.is_cancelled()
            })?;
            if !result.status.success() {
                return Err(ServerError::BadProject(format!(
                    "Lake setup-file failed ({}): {}",
                    result.status,
                    String::from_utf8_lossy(&result.stderr)
                )));
            }
            stage(&result.stdout, &writer, &sysroot)?;
            Ok(())
        })
        .await;
        on_drop.disarm();
        result.map_err(|e| ServerError::Internal(e.to_string()))?
    }
}
