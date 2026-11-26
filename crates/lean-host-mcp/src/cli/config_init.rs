//! `lean-host-mcp config init`: write a documented starter config file.
//!
//! The file body is generated from the one knob catalogue in
//! [`crate::config_schema`], so it always documents the current options at
//! their current defaults. By default it writes the tracked
//! `lean-host-mcp.toml` in the working directory; `--lake-root` writes it at
//! that root instead, and `--path` at an explicit location. It refuses to
//! overwrite an existing file unless `--force` is given.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};

use crate::config_file::TRACKED_FILE_NAME;
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

    #[test]
    fn lake_root_and_path_conflict() {
        let err = parse(&["init", "--lake-root", "/work", "--path", "/work/x.toml"]).expect_err("conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
