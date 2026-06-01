//! `lean-host-mcp config init`: write a documented starter config file.
//!
//! The file body is generated from the one knob catalogue in
//! [`crate::config_schema`], so it always documents the current options at
//! their current defaults. By default it writes a project-local
//! `lean-host-mcp.toml` in the working directory; `--home` targets the per-user
//! `~/.config/lean-host-mcp/config.toml` instead, and `--path` an explicit
//! location. It refuses to overwrite an existing file unless `--force` is given.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};

use crate::config_file::{LOCAL_FILE_NAME, home_config_path};
use crate::config_schema;

/// `config <command>` — generate and manage the config file.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write a documented starter config file with every option at its default.
    Init(ConfigInitArgs),
}

/// Flags for `config init`.
#[derive(Debug, Args)]
pub struct ConfigInitArgs {
    /// Write the per-user home config (`~/.config/lean-host-mcp/config.toml`)
    /// instead of a project-local `lean-host-mcp.toml`.
    #[arg(long, conflicts_with = "path")]
    pub home: bool,

    /// Write to an explicit path instead of the default location.
    #[arg(long, value_name = "FILE")]
    pub path: Option<PathBuf>,

    /// Overwrite the destination if it already exists.
    #[arg(long)]
    pub force: bool,
}

/// Entry point invoked from `main.rs`.
///
/// # Errors
///
/// Returns an error when the destination exists and `--force` was not given, or
/// when the parent directory or file cannot be written.
pub fn run(command: &ConfigCommand) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Init(args) => run_init(args),
    }
}

fn run_init(args: &ConfigInitArgs) -> anyhow::Result<()> {
    let dest = resolve_dest(args)?;
    if dest.exists() && !args.force {
        bail!("{} already exists; pass --force to overwrite it", dest.display());
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating config directory {}", parent.display()))?;
    }
    fs::write(&dest, config_schema::render_default_toml())
        .with_context(|| format!("writing config to {}", dest.display()))?;
    println!("wrote {}", dest.display());
    println!("Edit it, then launch the server from this directory (or pass --lake-root).");
    Ok(())
}

/// Resolve where to write: an explicit `--path`, else the home config under
/// `--home`, else the project-local file name in the current directory.
fn resolve_dest(args: &ConfigInitArgs) -> anyhow::Result<PathBuf> {
    if let Some(path) = args.path.as_deref() {
        return Ok(path.to_path_buf());
    }
    if args.home {
        return home_config_path()
            .context("could not determine a home config directory; pass --path to choose a location");
    }
    Ok(PathBuf::from(LOCAL_FILE_NAME))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use clap::{Command, FromArgMatches};

    fn parse(args: &[&str]) -> Result<ConfigInitArgs, clap::Error> {
        let matches = ConfigInitArgs::augment_args(Command::new("init")).try_get_matches_from(args)?;
        ConfigInitArgs::from_arg_matches(&matches)
    }

    #[test]
    fn default_destination_is_the_project_local_file() {
        let args = parse(&["init"]).expect("bare init parses");
        assert_eq!(resolve_dest(&args).expect("dest"), PathBuf::from(LOCAL_FILE_NAME));
    }

    #[test]
    fn explicit_path_wins_over_home() {
        let args = parse(&["init", "--path", "/tmp/custom.toml"]).expect("path parses");
        assert_eq!(resolve_dest(&args).expect("dest"), PathBuf::from("/tmp/custom.toml"));
    }

    #[test]
    fn home_and_path_conflict() {
        let err = parse(&["init", "--home", "--path", "/tmp/x.toml"]).expect_err("home + path conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
