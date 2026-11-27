//! `lean-host-mcp config init` and `config check`.
//!
//! `init` writes a documented starter config file. The file body is generated
//! from the one knob catalogue in [`crate::config_schema`], so it always
//! documents the current options at their current defaults. By default it
//! writes the tracked `lean-host-mcp.toml` in the working directory;
//! `--lake-root` writes it at that root instead, and `--path` at an explicit
//! location. It refuses to overwrite an existing file unless `--force` is
//! given.
//!
//! `check` reads a Lake root's config files, names every unknown key and bad
//! value, and prints the build tables as builds read them. It writes nothing.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};

use crate::build::config::BuildConfig;
use crate::config_file::{self, ConfigFile, TRACKED_FILE_NAME};
use crate::config_schema;

/// `config <command>` — generate and check the config file.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write a documented starter config file with every option at its default.
    Init(ConfigInitArgs),
    /// Check a Lake root's config files and print its build tables. Writes nothing.
    Check(ConfigCheckArgs),
}

/// Flags for `config init`.
#[derive(Debug, Args)]
pub struct ConfigInitArgs {
    /// Write `lean-host-mcp.toml` at this Lake root instead of the working
    /// directory.
    #[arg(long, value_name = "DIR", conflicts_with = "path")]
    pub lake_root: Option<PathBuf>,

    /// Write to an explicit path instead of the default location.
    #[arg(long, value_name = "FILE")]
    pub path: Option<PathBuf>,

    /// Overwrite the destination if it already exists.
    #[arg(long)]
    pub force: bool,
}

/// Flags for `config check`.
#[derive(Debug, Args)]
pub struct ConfigCheckArgs {
    /// The Lake root whose config files to check.
    #[arg(long, value_name = "DIR")]
    pub lake_root: PathBuf,
}

/// Entry point invoked from `main.rs`.
///
/// # Errors
///
/// `init` fails when the destination exists and `--force` was not given, or
/// when the parent directory or file cannot be written. `check` fails when
/// the root does not resolve or its config has a problem, and names each.
pub fn run(command: &ConfigCommand) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Init(args) => run_init(args),
        ConfigCommand::Check(args) => {
            let root = args
                .lake_root
                .canonicalize()
                .with_context(|| format!("--lake-root {}", args.lake_root.display()))?;
            let report = check(&root).map_err(|problems| anyhow::anyhow!(problems.join("\n")))?;
            print!("{report}");
            Ok(())
        }
    }
}

/// Check the config files of Lake root `root` and describe them: which files
/// exist, and the build tables with every default filled in.
///
/// # Errors
///
/// Names each unreadable file, unknown table or key, and bad value.
fn check(root: &Path) -> Result<String, Vec<String>> {
    let mut report = String::new();
    for path in [root.join(TRACKED_FILE_NAME), config_file::local_file(root)] {
        let state = if path.exists() { "read" } else { "absent" };
        let _ = writeln!(report, "{state}: {}", path.display());
    }
    let table = config_file::load_strict(root).map_err(|error| vec![error])?;
    let mut problems = Vec::new();
    for (name, value) in &table {
        if name == "build" || name == "recipes" {
            continue;
        }
        if !config_schema::is_section(name) {
            problems.push(format!("unknown table `[{name}]`"));
            continue;
        }
        let Some(keys) = value.as_table() else {
            problems.push(format!("`{name}` must be a table"));
            continue;
        };
        for key in keys.keys() {
            if !config_schema::is_documented(&format!("{name}.{key}")) {
                problems.push(format!("[{name}]: unknown key `{key}`"));
            }
        }
    }
    if let Err(error) = toml::Value::Table(table.clone()).try_into::<ConfigFile>() {
        problems.push(error.to_string().trim_end().to_owned());
    }
    match BuildConfig::from_table(&table) {
        Ok(build) if problems.is_empty() => {
            let rendered = build.render().map_err(|error| vec![error.to_string()])?;
            let _ = write!(report, "\n# The build tables as builds read them:\n{rendered}");
            Ok(report)
        }
        Ok(_) => Err(problems),
        Err(errors) => {
            problems.extend(errors);
            Err(problems)
        }
    }
}

fn run_init(args: &ConfigInitArgs) -> anyhow::Result<()> {
    let dest = resolve_dest(args);
    if dest.exists() && !args.force {
        bail!("{} already exists; pass --force to overwrite it", dest.display());
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating config directory {}", parent.display()))?;
    }
    fs::write(&dest, config_schema::render_default_toml())
        .with_context(|| format!("writing config to {}", dest.display()))?;
    println!("wrote {}", dest.display());
    println!(
        "The server reads {TRACKED_FILE_NAME} at its --lake-root; keys in .local/lean-host-mcp/config.toml there win."
    );
    Ok(())
}

/// Resolve where to write: an explicit `--path`, else the tracked file at
/// `--lake-root`, else the tracked file name in the current directory.
fn resolve_dest(args: &ConfigInitArgs) -> PathBuf {
    if let Some(path) = args.path.as_deref() {
        return path.to_path_buf();
    }
    args.lake_root
        .as_deref()
        .map_or_else(|| PathBuf::from(TRACKED_FILE_NAME), |root| root.join(TRACKED_FILE_NAME))
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert the branch under test directly")]
mod tests {
    use super::*;
    use clap::{Command, FromArgMatches};

    fn parse(args: &[&str]) -> Result<ConfigInitArgs, clap::Error> {
        let matches = ConfigInitArgs::augment_args(Command::new("init")).try_get_matches_from(args)?;
        ConfigInitArgs::from_arg_matches(&matches)
    }

    #[test]
    fn default_destination_is_the_tracked_file() {
        let args = parse(&["init"]).expect("bare init parses");
        assert_eq!(resolve_dest(&args), PathBuf::from(TRACKED_FILE_NAME));
    }

    #[test]
    fn lake_root_destination_is_the_tracked_file_there() {
        let args = parse(&["init", "--lake-root", "/work/project"]).expect("lake root parses");
        assert_eq!(resolve_dest(&args), PathBuf::from("/work/project/lean-host-mcp.toml"));
    }

    #[test]
    fn explicit_path_is_used_as_given() {
        let args = parse(&["init", "--path", "/work/custom.toml"]).expect("path parses");
        assert_eq!(resolve_dest(&args), PathBuf::from("/work/custom.toml"));
    }

    fn root_with(tracked: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        fs::write(root.join(TRACKED_FILE_NAME), tracked).expect("write config");
        (dir, root)
    }

    fn names(root: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(root)
            .expect("list root")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn check_names_every_problem_and_writes_nothing() {
        let (_dir, root) = root_with(
            "[launcher]\nmode = 'x'\n[broker]\nmax_worker = 2\nmax_projects = 3\n\
             [runtime]\nlean_max_memory_kib = 'big'\n[build]\ntimeout_secs = 0\n\
             [recipes.lint]\nsteps = [[]]\n",
        );
        let before = names(&root);
        let problems = check(&root).expect_err("problems");
        let named = |text: &str| problems.iter().any(|problem| problem.contains(text));
        assert!(named("[launcher]"), "{problems:?}");
        assert!(named("`max_worker`"), "{problems:?}");
        assert!(named("[broker]: unknown key `max_projects`"), "{problems:?}");
        assert!(named("lean_max_memory_kib"), "{problems:?}");
        assert!(named("[build]: `timeout_secs`"), "{problems:?}");
        assert!(named("[recipes.lint]: step 0 is empty"), "{problems:?}");
        assert_eq!(names(&root), before);
    }

    #[test]
    fn check_prints_the_build_tables_with_their_defaults() {
        let (_dir, root) = root_with("[build]\nguard = ['Dep']\n[recipes.lint]\nsteps = [['lint']]\n");
        let report = check(&root).expect("clean config");
        assert!(report.contains(&format!("read: {}", root.join(TRACKED_FILE_NAME).display())));
        assert!(report.contains(&format!("absent: {}", config_file::local_file(&root).display())));
        let (_, tables) = report.split_once("read them:\n").expect("build tables");
        let tables: toml::Table = toml::from_str(tables).expect("tables are TOML");
        assert_eq!(
            BuildConfig::from_table(&tables).expect("tables check"),
            BuildConfig::from_table(
                &toml::from_str("[build]\nguard = ['Dep']\n[recipes.lint]\nsteps = [['lint']]\n").expect("toml")
            )
            .expect("source checks")
        );
        let guard = tables
            .get("recipes")
            .and_then(|recipes| recipes.get("lint"))
            .and_then(|lint| lint.get("guard"))
            .and_then(toml::Value::as_str);
        assert_eq!(guard, Some("before"));
    }

    #[test]
    fn lake_root_and_path_conflict() {
        let err = parse(&["init", "--lake-root", "/work", "--path", "/work/x.toml"]).expect_err("conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
