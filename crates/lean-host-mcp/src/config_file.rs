//! TOML config files holding every tunable knob.
//!
//! The server reads two files from its Lake root (`--lake-root`): first the
//! tracked `<root>/lean-host-mcp.toml`, then the ignored
//! `<root>/.local/lean-host-mcp/config.toml` that `lean-host-mcp install`
//! writes. They merge **per key** and the local file wins, so each file need
//! only state the knobs it changes. Without a Lake root the server reads no
//! file. Nothing here reads the home or the working directory.
//!
//! The files are one layer in the precedence stack
//! `CLI > env > local file > tracked file > default`: every field here is
//! `Option`, and a set env var still wins over a file value. Every setting
//! reads its env var through [`env_value`], so an empty one counts as unset
//! everywhere, and parses it through [`setting`]. A missing file contributes
//! nothing. A file that cannot be read or parsed, or a value of the wrong
//! type, is an error that names the file: the server refuses to start rather
//! than serve with the defaults.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

/// Tracked file name at the Lake root. Also the file `config init` writes.
pub(crate) const TRACKED_FILE_NAME: &str = "lean-host-mcp.toml";

/// Ignored local file name inside [`local_dir`]. `lean-host-mcp install`
/// writes it.
pub(crate) const LOCAL_FILE_NAME: &str = "config.toml";

/// `<root>/.local/lean-host-mcp`: the machine-local directory for one Lake
/// root. It holds the local config file and, by default, the installed
/// binaries, workers and server store.
#[must_use]
pub(crate) fn local_dir(root: &Path) -> PathBuf {
    root.join(".local").join("lean-host-mcp")
}

/// `<root>/.local/lean-host-mcp/config.toml`.
#[must_use]
pub(crate) fn local_file(root: &Path) -> PathBuf {
    local_dir(root).join(LOCAL_FILE_NAME)
}

/// Create [`local_dir`] for `root` with a `.gitignore` that ignores all of it,
/// so what install and the server write there never shows in `git status`,
/// even in a project that does not ignore `.local/`.
///
/// # Errors
///
/// Returns the I/O error when the directory or its `.gitignore` cannot be
/// written.
pub(crate) fn ensure_local_dir(root: &Path) -> io::Result<PathBuf> {
    let dir = local_dir(root);
    fs::create_dir_all(&dir)?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"))
    {
        Ok(mut file) => file.write_all(b"*\n")?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    Ok(dir)
}

/// Reads one env var. The server passes [`process_env`]; tests pass a fixed
/// list, so they need not set the process environment.
pub(crate) type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The process environment, as an [`Env`].
pub(crate) fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The value of env var `name`, or `None` when it is unset, empty or only
/// white space.
pub(crate) fn env_value(env: Env<'_>, name: &str) -> Option<OsString> {
    env(name).filter(|value| !value.to_string_lossy().trim().is_empty())
}

/// One setting by `env var > config file`: the trimmed value of env var
/// `name` parsed as `T`, else `file`. The caller applies the default.
///
/// # Errors
///
/// Names the variable when its value is not UTF-8 or does not parse.
pub(crate) fn setting<T>(env: Env<'_>, name: &str, file: Option<T>) -> Result<Option<T>, String>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    let Some(value) = env_value(env, name) else {
        return Ok(file);
    };
    let text = value
        .to_str()
        .ok_or_else(|| format!("{name} is not valid UTF-8"))?
        .trim();
    text.parse()
        .map(Some)
        .map_err(|error| format!("{name}={text:?} does not parse: {error}"))
}

/// [`setting`] with `default` filled in, for a value that must be positive.
///
/// # Errors
///
/// As [`setting`], and names the variable when the value from either source
/// is zero.
pub(crate) fn positive_setting<T>(env: Env<'_>, name: &str, file: Option<T>, default: T) -> Result<T, String>
where
    T: FromStr + PartialEq + From<u8>,
    T::Err: fmt::Display,
{
    let value = setting(env, name, file)?.unwrap_or(default);
    if value == T::from(0) {
        return Err(format!(
            "{name} is 0, from the env var or the config file; it must be positive"
        ));
    }
    Ok(value)
}

/// A directory by `CLI flag > env var > config file > default`: `flag`, then
/// env var `name`, then what `configured` reads from the config files, then
/// `default`.
///
/// # Errors
///
/// Returns the error `configured` returns.
pub(crate) fn pick_dir(
    flag: Option<&Path>,
    env: Env<'_>,
    name: &str,
    configured: impl FnOnce() -> Result<Option<PathBuf>, String>,
    default: impl FnOnce() -> PathBuf,
) -> Result<PathBuf, String> {
    if let Some(flag) = flag {
        return Ok(flag.to_path_buf());
    }
    if let Some(value) = env_value(env, name) {
        return Ok(PathBuf::from(value));
    }
    Ok(configured()?.unwrap_or_else(default))
}

/// Merged view of the config files. All fields optional: an absent field
/// defers to the env var, then the built-in default.
#[derive(Debug, Default, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub runtime: RuntimeFileConfig,
    #[serde(default)]
    pub broker: BrokerFileConfig,
    #[serde(default)]
    pub server: ServerFileConfig,
    #[serde(default)]
    pub telemetry: TelemetryFileConfig,
    #[serde(default)]
    pub output: OutputFileConfig,
    #[serde(default)]
    pub install: InstallFileConfig,
    #[serde(default)]
    pub requests: RequestsFileConfig,
}

/// `[requests]` — how long the request table keeps ended requests, and how
/// long one `lean_get` may wait.
#[derive(Debug, Default, Deserialize)]
pub struct RequestsFileConfig {
    /// Hours an ended request stays in the table.
    pub retention_hours: Option<u64>,
    /// The most rows the table keeps; the oldest ended rows go first.
    pub max_rows: Option<u64>,
    /// The longest `wait_seconds` a `lean_get` may ask for.
    pub wait_max_secs: Option<u64>,
}

/// `[runtime]` — worker policy knobs (mirrors `ProjectRuntimeConfig`).
#[derive(Debug, Default, Deserialize)]
pub struct RuntimeFileConfig {
    pub lean_max_memory_kib: Option<u64>,
    pub request_timeout_millis: Option<u64>,
    pub project_mailbox_capacity: Option<usize>,
    pub worker_restart_limit: Option<usize>,
    pub worker_restart_window_secs: Option<u64>,
    pub worker_import_residue_budget_mib: Option<u64>,
    pub worker_session_pool_capacity: Option<usize>,
}

/// `[broker]` — worker-pool knobs.
#[derive(Debug, Default, Deserialize)]
pub struct BrokerFileConfig {
    pub max_workers: Option<usize>,
    pub max_workers_per_project: Option<usize>,
    pub max_lean_runs: Option<usize>,
    pub idle_timeout_secs: Option<u64>,
}

/// `[server]` — transport knobs.
///
/// `lake_root` is absent: the files live in the Lake root, so the root must be
/// known before either is read. `bind` is a raw string (e.g.
/// `"127.0.0.1:8765"`); the binary parses and validates it as a `SocketAddr`,
/// so this library schema stays free of transport types.
#[derive(Debug, Default, Deserialize)]
pub struct ServerFileConfig {
    pub bind: Option<String>,
    pub http_path: Option<String>,
    /// Which field of the tool result carries the envelope: `text` (default),
    /// `structured`, or `both`. See `crate::tools::ResponseCarrier`.
    pub response_carrier: Option<String>,
}

/// `[telemetry]` — how much operational telemetry the model-facing envelope
/// carries (`quiet` default, or `full`). See `crate::tools::TelemetryVerbosity`.
#[derive(Debug, Default, Deserialize)]
pub struct TelemetryFileConfig {
    pub verbosity: Option<String>,
}

/// `[output]` — server-wide output budget overrides.
///
/// These replace the former per-call `max_field_bytes` / `max_total_bytes` /
/// `heartbeat_limit` request arguments. Each is optional: unset keeps the
/// per-tool built-in default.
#[derive(Debug, Default, Deserialize)]
pub struct OutputFileConfig {
    pub max_field_bytes: Option<u32>,
    pub max_total_bytes: Option<u32>,
    pub heartbeat_limit: Option<u64>,
    pub slow_call_warning_millis: Option<u64>,
}

/// `[install]` — the paths `lean-host-mcp install` records in the local file.
///
/// A relative path is relative to the Lake root.
#[derive(Debug, Default, Deserialize)]
pub struct InstallFileConfig {
    /// The installed server executable, for MCP client configs and scripts
    /// that run the server by absolute path. The server itself ignores it.
    pub binary: Option<PathBuf>,
    /// Directory of per-toolchain worker installs.
    pub workers_dir: Option<PathBuf>,
    /// Private directory for server state, including the lock that admits
    /// one server.
    pub store_dir: Option<PathBuf>,
    /// Elan's home when install ran. The server finds the Lean toolchain
    /// there when `ELAN_HOME` is unset, so it does not depend on `HOME`.
    pub elan_home: Option<PathBuf>,
}

impl InstallFileConfig {
    /// The configured worker directory, resolved against `root`.
    #[must_use]
    pub fn workers_dir(&self, root: &Path) -> Option<PathBuf> {
        self.workers_dir.as_deref().map(|path| root.join(path))
    }

    /// The recorded elan home, resolved against `root`.
    #[must_use]
    pub fn elan_home(&self, root: &Path) -> Option<PathBuf> {
        self.elan_home.as_deref().map(|path| root.join(path))
    }

    /// The configured store directory, resolved against `root`.
    #[must_use]
    pub fn store_dir(&self, root: &Path) -> Option<PathBuf> {
        self.store_dir.as_deref().map(|path| root.join(path))
    }
}

impl ConfigFile {
    /// The config files of the Lake root `root`, merged: the tracked file is
    /// the base and the local file overlays it per key. `None` reads nothing
    /// and gives the empty config.
    ///
    /// # Errors
    ///
    /// Names the file that cannot be read or parsed, or the value whose type
    /// does not match the schema.
    pub fn load(root: Option<&Path>) -> Result<Self, String> {
        match root {
            Some(root) => Self::from_table(load_table(root)?),
            None => Ok(Self::default()),
        }
    }

    /// The settings in a merged config table. Tables this schema does not
    /// name, such as `[build]`, are left to their own readers.
    ///
    /// # Errors
    ///
    /// Names the value whose type does not match the schema.
    pub(crate) fn from_table(table: toml::Table) -> Result<Self, String> {
        toml::Value::Table(table)
            .try_into()
            .map_err(|error: toml::de::Error| format!("config: {}", error.to_string().trim_end()))
    }
}

/// The config files of Lake root `root`, merged: the tracked file is the base
/// and the local file overlays it per key. A missing file contributes
/// nothing. Every reader of the files starts here.
///
/// # Errors
///
/// Names the file and the read or parse error.
pub(crate) fn load_table(root: &Path) -> Result<toml::Table, String> {
    let mut merged = toml::Value::Table(toml::Table::new());
    for path in [root.join(TRACKED_FILE_NAME), local_file(root)] {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let value = toml::from_str::<toml::Value>(&contents).map_err(|error| format!("{}: {error}", path.display()))?;
        merge_toml(&mut merged, value);
    }
    match merged {
        toml::Value::Table(table) => Ok(table),
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_)
        | toml::Value::Array(_) => Err("merged config is not a table".to_owned()),
    }
}

/// Deep-merge `overlay` onto `base` in place: tables merge key-by-key
/// (recursing), any non-table value replaces wholesale. So `[runtime]` in the
/// local file overrides only the keys it sets, leaving sibling tracked keys.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                match base_table.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base_table.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert the branch under test directly"
)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir(&root).unwrap();
        (dir, root)
    }

    #[test]
    fn local_file_overrides_the_tracked_file_per_key() {
        let (_dir, root) = project();
        fs::write(
            root.join(TRACKED_FILE_NAME),
            "[runtime]\nlean_max_memory_kib = 5\n[broker]\nmax_workers = 8\n",
        )
        .unwrap();
        ensure_local_dir(&root).unwrap();
        fs::write(
            local_file(&root),
            "[runtime]\nlean_max_memory_kib = 8\nrequest_timeout_millis = 16\n",
        )
        .unwrap();
        let config = ConfigFile::load(Some(&root)).unwrap();

        // Local overrode the one runtime key it set...
        assert_eq!(config.runtime.lean_max_memory_kib, Some(8));
        // ...added its own...
        assert_eq!(config.runtime.request_timeout_millis, Some(16));
        // ...and left the other tracked keys in place.
        assert_eq!(config.broker.max_workers, Some(8));
    }

    #[test]
    fn only_the_lake_root_is_read() {
        let (dir, root) = project();
        // A file in an ancestor of the root, or in the working directory, is
        // not a config file of this root.
        fs::write(dir.path().join(TRACKED_FILE_NAME), "[broker]\nmax_workers = 3\n").unwrap();
        assert!(ConfigFile::load(Some(&root)).unwrap().broker.max_workers.is_none());
        assert!(ConfigFile::load(None).unwrap().broker.max_workers.is_none());
    }

    #[test]
    fn install_paths_resolve_against_the_root() {
        let (_dir, root) = project();
        fs::write(root.join(TRACKED_FILE_NAME), "[install]\nworkers_dir = 'workers'\n").unwrap();
        ensure_local_dir(&root).unwrap();
        fs::write(local_file(&root), "[install]\nstore_dir = '/state/store'\n").unwrap();
        let install = ConfigFile::load(Some(&root)).unwrap().install;
        assert_eq!(install.workers_dir(&root), Some(root.join("workers")));
        assert_eq!(install.store_dir(&root), Some(PathBuf::from("/state/store")));
        assert!(install.binary.is_none());
    }

    /// An env lookup that knows only `NAME`, set to `value`.
    fn one_var(value: &'static str) -> impl Fn(&str) -> Option<OsString> {
        move |name| (name == "NAME").then(|| OsString::from(value))
    }

    #[test]
    fn pick_dir_follows_flag_env_file_default() {
        let flag = Path::new("/flag");
        let env = one_var("/env");
        let file = || Ok(Some(PathBuf::from("/file")));
        let default = || PathBuf::from("/default");
        let pick = |flag, env: Env<'_>, file: fn() -> Result<Option<PathBuf>, String>| {
            pick_dir(flag, env, "NAME", file, default).unwrap()
        };
        assert_eq!(pick(Some(flag), &env, file), flag);
        assert_eq!(pick(None, &env, file), Path::new("/env"));
        assert_eq!(pick(None, &one_var(""), file), Path::new("/file"));
        assert_eq!(pick(None, &one_var(" \n"), file), Path::new("/file"));
        assert_eq!(pick(None, &|_| None, file), Path::new("/file"));
        assert_eq!(pick(None, &|_| None, || Ok(None)), Path::new("/default"));
        assert_eq!(
            pick_dir(None, &|_| None, "NAME", || Err("bad file".to_owned()), default),
            Err("bad file".to_owned())
        );
    }

    #[test]
    fn a_setting_takes_a_set_env_var_over_the_file() {
        assert_eq!(setting::<u64>(&one_var(" 7 "), "NAME", Some(3)), Ok(Some(7)));
        assert_eq!(setting::<u64>(&|_| None, "NAME", Some(3)), Ok(Some(3)));
        assert_eq!(setting::<u64>(&|_| None, "NAME", None), Ok(None));
    }

    /// An empty env var counts as unset for every setting, whatever its type.
    #[test]
    fn a_setting_treats_an_empty_env_var_as_unset() {
        for value in ["", "  ", "\t\n"] {
            assert_eq!(setting::<u64>(&one_var(value), "NAME", Some(3)), Ok(Some(3)));
            assert_eq!(setting::<String>(&one_var(value), "NAME", None), Ok(None));
        }
    }

    #[test]
    fn a_setting_names_the_env_var_that_does_not_parse() {
        let error = setting::<u64>(&one_var("seven"), "NAME", Some(3)).unwrap_err();
        assert!(error.starts_with("NAME=\"seven\""), "{error}");
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_naming_it() {
        let (_dir, root) = project();
        fs::write(root.join(TRACKED_FILE_NAME), "[broker\nmax_workers = 3\n").unwrap();
        let error = ConfigFile::load(Some(&root)).unwrap_err();
        assert!(error.contains(TRACKED_FILE_NAME), "{error}");
    }

    /// A value of the wrong type stops the load. The old loader dropped the
    /// whole config instead, so one typo put every setting back to its default.
    #[test]
    fn a_value_of_the_wrong_type_is_an_error_naming_its_key() {
        let (_dir, root) = project();
        fs::write(
            root.join(TRACKED_FILE_NAME),
            "[runtime]\nlean_max_memory_kib = 5\n[broker]\nmax_workers = 'eight'\n",
        )
        .unwrap();
        let error = ConfigFile::load(Some(&root)).unwrap_err();
        assert!(error.contains("max_workers"), "{error}");
    }

    #[test]
    fn local_dir_ignores_itself() {
        let (_dir, root) = project();
        let dir = ensure_local_dir(&root).unwrap();
        assert_eq!(dir, root.join(".local/lean-host-mcp"));
        assert_eq!(fs::read_to_string(dir.join(".gitignore")).unwrap(), "*\n");
        // A second call keeps an edited ignore file.
        fs::write(dir.join(".gitignore"), "*\n!keep\n").unwrap();
        ensure_local_dir(&root).unwrap();
        assert_eq!(fs::read_to_string(dir.join(".gitignore")).unwrap(), "*\n!keep\n");
    }

    #[test]
    fn unknown_tables_are_ignored() {
        let (_dir, root) = project();
        fs::write(
            root.join(TRACKED_FILE_NAME),
            "[launcher]\nmode = 'managed'\n[broker]\nmax_workers = 2\n",
        )
        .unwrap();
        assert_eq!(ConfigFile::load(Some(&root)).unwrap().broker.max_workers, Some(2));
    }

    #[test]
    fn full_example_parses() {
        let config: ConfigFile = toml::from_str::<toml::Value>(
            "[runtime]\nlean_max_memory_kib = 8388608\nworker_restart_window_secs = 60\n\
             [broker]\nmax_workers = 4\nidle_timeout_secs = 600\n\
             [server]\nbind = \"127.0.0.1:8765\"\nhttp_path = \"/mcp\"\n",
        )
        .unwrap()
        .try_into()
        .unwrap();

        assert_eq!(config.runtime.lean_max_memory_kib, Some(8_388_608));
        assert_eq!(config.broker.max_workers, Some(4));
        assert_eq!(config.broker.idle_timeout_secs, Some(600));
        assert_eq!(config.server.bind.as_deref(), Some("127.0.0.1:8765"));
        assert_eq!(config.server.http_path.as_deref(), Some("/mcp"));
    }

    #[test]
    fn empty_config_is_all_none() {
        let config = ConfigFile::default();
        assert!(config.runtime.lean_max_memory_kib.is_none());
        assert!(config.broker.max_workers.is_none());
        assert!(config.server.bind.is_none());
        assert!(config.install.workers_dir.is_none());
    }
}
