use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::config::{Append, BuildConfig, DEFAULT_LOG_LIMIT_MIB, DEFAULT_TIMEOUT_SECS, GuardPlace};
use super::{BuildPlan, BuildRequest, log_path, plan, read_tail};

/// A reduced copy of the Proofs recipes, which must keep loading.
const PROOFS: &str = r#"
[build]
guard = ["@mathlib/+Mathlib:olean"]
timeout_secs = 14400
log_limit_mib = 16

[recipes.cache]
steps = [["exe", "cache", "get"]]
guard = "after"

[recipes.lint]
steps = [
  ["build", "Proofs", "runLinter", "style-lint"],
  ["--no-build", "--no-cache", "build", "@mathlib/lint-style"],
  ["--no-build", "--no-cache", "lint", "--", "--no-build"],
  ["--no-build", "--no-cache", "exe", "@mathlib/lint-style"],
]

[recipes.format]
steps = [
  ["build"],
  ["exe", "lean-fmt", "format", "--check", "--no-build", "--inherit-process-group", "--workers", "2"],
]
append = "each"

[recipes.strict-lint]
steps = [["--no-build", "exe", "runLinter", "--no-build", "--module-only", "--format=json", "--deny=all"]]
append = "last"

[recipes.proof-audit]
steps = [["--no-build", "exe", "runLinter", "--no-build", "--only=largeProofTermLinter", "--proof-branch-depth=8"]]
append = "last"
"#;

fn config(text: &str) -> Result<BuildConfig, String> {
    let table: toml::Table = toml::from_str(text).map_err(|error| error.to_string())?;
    BuildConfig::from_table(&table).map_err(|errors| errors.join("\n"))
}

fn errors(text: &str) -> Vec<String> {
    toml::from_str(text)
        .map_err(|error: toml::de::Error| vec![error.to_string()])
        .and_then(|table: toml::Table| BuildConfig::from_table(&table))
        .err()
        .unwrap_or_default()
}

fn words(text: &str) -> Vec<String> {
    text.split(' ').map(str::to_owned).collect()
}

fn recipe(name: &str, args: &str) -> BuildRequest {
    BuildRequest {
        recipe: Some(name.to_owned()),
        args: if args.is_empty() { Vec::new() } else { words(args) },
        ..BuildRequest::default()
    }
}

fn root() -> io::Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    Ok((directory, root))
}

#[test]
fn the_proofs_recipes_load_with_their_defaults() -> Result<(), String> {
    let config = config(PROOFS)?;
    assert_eq!(config.build.guard, ["@mathlib/+Mathlib:olean"]);
    assert_eq!(
        config.recipes.keys().map(String::as_str).collect::<Vec<_>>(),
        ["cache", "format", "lint", "proof-audit", "strict-lint"]
    );
    let cache = config.recipes.get("cache").ok_or("cache")?;
    assert_eq!((cache.append, cache.guard), (Append::None, GuardPlace::After));
    let audit = config.recipes.get("proof-audit").ok_or("proof-audit")?;
    assert_eq!((audit.append, audit.guard), (Append::Last, GuardPlace::Before));
    let rendered = config.render().map_err(|error| error.to_string())?;
    assert_eq!(self::config(&rendered)?, config);
    Ok(())
}

#[test]
fn an_empty_config_has_the_default_build_table() -> Result<(), String> {
    let config = config("")?;
    assert!(config.build.guard.is_empty() && config.recipes.is_empty());
    assert_eq!(
        (config.build.timeout_secs, config.build.log_limit_mib),
        (DEFAULT_TIMEOUT_SECS, DEFAULT_LOG_LIMIT_MIB)
    );
    Ok(())
}

#[test]
fn every_problem_is_named_at_once() {
    let found = errors(
        r#"
[build]
guard = ["-d"]
timeout_secs = 0
env = { LEAN_NUM_THREADS = "2" }

[recipes.typo]
step = [["build"]]

[recipes.bad]
steps = [["build", "-d", "/elsewhere"], ["--no-build"], []]

[recipes.cut]
steps = [["build"]]
mode = "all"
"#,
    );
    for expected in [
        "[build]: unknown field `env`",
        "[recipes.typo]: unknown field `step`",
        "[recipes.bad]: step 0: `-d` would move Lake",
        "[recipes.bad]: step 1 names no Lake command",
        "[recipes.bad]: step 2 names no Lake command",
        "[recipes.cut]: unknown field `mode`",
    ] {
        assert!(
            found.iter().any(|error| error.starts_with(expected)),
            "{expected} not in {found:#?}"
        );
    }
    // Unknown fields stop the rest of the table from loading, so `[build]`
    // reports only that one.
    assert!(
        !found.iter().any(|error| error.contains("must be at least 1")),
        "{found:#?}"
    );
}

#[test]
fn build_values_are_checked_once_the_table_loads() {
    let found = errors(
        r#"
[build]
guard = ["", "-d"]
timeout_secs = 0
log_limit_mib = 0
"#,
    );
    assert_eq!(found.len(), 4, "{found:#?}");
    for expected in [
        "guard target ``",
        "guard target `-d`",
        "`timeout_secs` must be at least 1",
        "`log_limit_mib` must be at least 1",
    ] {
        assert!(
            found.iter().any(|error| error.contains(expected)),
            "{expected} not in {found:#?}"
        );
    }
}

#[test]
fn recipes_must_be_tables() {
    assert_eq!(errors("recipes = 3"), ["`recipes` must be a table of recipe tables"]);
    assert!(
        errors("[recipes]\nlint = 3")
            .iter()
            .any(|error| error.starts_with("[recipes.lint]"))
    );
}

#[test]
fn targets_build_after_the_guard() -> Result<(), String> {
    let config = config(PROOFS)?;
    let request = BuildRequest {
        targets: words("Proofs.A Proofs.B"),
        ..BuildRequest::default()
    };
    let planned = plan(&config, &request)?;
    assert_eq!(
        planned,
        BuildPlan {
            steps: vec![
                words("build --no-build @mathlib/+Mathlib:olean"),
                words("build Proofs.A Proofs.B"),
            ],
            timeout: Duration::from_secs(14_400),
            log_limit: 16 << 20,
            tail_bytes: 8 << 10,
        }
    );
    // Without targets Lake builds its default targets; without a guard the
    // build is the only step.
    let bare = plan(&self::config("")?, &BuildRequest::default())?;
    assert_eq!(bare.steps, [words("build")]);
    Ok(())
}

#[test]
fn requests_that_do_not_fit_are_refused() -> Result<(), String> {
    let config = config(PROOFS)?;
    let refused = |request: &BuildRequest| plan(&config, request).err().unwrap_or_default();
    let options = BuildRequest {
        targets: words("Proofs --wfail"),
        ..BuildRequest::default()
    };
    assert!(refused(&options).contains("`--wfail` starts with `-`"));
    let args = BuildRequest {
        args: words("x"),
        ..BuildRequest::default()
    };
    assert!(refused(&args).contains("needs a `recipe`"));
    let both = BuildRequest {
        targets: words("Proofs"),
        ..recipe("lint", "")
    };
    assert!(refused(&both).contains("exclude each other"));
    assert_eq!(
        refused(&recipe("nope", "")),
        "no recipe `nope`; this project has cache, format, lint, proof-audit, strict-lint"
    );
    assert!(refused(&recipe("lint", "x")).contains("takes no `args`"));
    // `format` appends to `build` too, where Lake would read the option.
    assert!(refused(&recipe("format", "--wfail")).contains("step 0"));
    Ok(())
}

#[test]
fn recipes_place_the_guard_and_the_args() -> Result<(), String> {
    let config = config(PROOFS)?;
    let guard = words("build --no-build @mathlib/+Mathlib:olean");
    let cache = plan(&config, &recipe("cache", ""))?;
    assert_eq!(cache.steps, [words("exe cache get"), guard.clone()]);
    let format = plan(&config, &recipe("format", "Proofs/A.lean"))?;
    assert_eq!(
        format.steps,
        [
            guard,
            words("build Proofs/A.lean"),
            words("exe lean-fmt format --check --no-build --inherit-process-group --workers 2 Proofs/A.lean"),
        ]
    );
    // Words that start with `-` fit after `exe runLinter`.
    let strict = plan(&config, &recipe("strict-lint", "--json-out Proofs.A"))?;
    assert_eq!(
        strict.steps.last().map(|step| step.join(" ")),
        Some(
            "--no-build exe runLinter --no-build --module-only --format=json --deny=all --json-out Proofs.A".to_owned()
        )
    );
    let unguarded = self::config("[recipes.clean]\nsteps = [[\"clean\", \"Proofs\"]]\nguard = \"none\"")?;
    assert_eq!(plan(&unguarded, &recipe("clean", ""))?.steps, [words("clean Proofs")]);
    Ok(())
}

#[test]
fn the_tail_never_exceeds_the_output_cap() -> Result<(), String> {
    let mut planned = plan(&config("")?, &BuildRequest::default())?;
    planned.cap_tail(None);
    assert_eq!(planned.tail_bytes, 8 << 10);
    planned.cap_tail(Some(1024));
    assert_eq!(planned.tail_bytes, 1024);
    planned.cap_tail(Some(1 << 20));
    assert_eq!(planned.tail_bytes, 1024);
    Ok(())
}

#[test]
fn logs_live_in_a_private_directory_under_hashed_names() -> io::Result<()> {
    let (_directory, store) = root()?;
    let first = log_path(&store, "srv-1")?;
    assert!(first.is_absolute());
    assert_eq!(first.parent(), Some(store.join("logs").as_path()));
    let name = first.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    assert_eq!(name.len(), 36);
    assert_eq!(Path::new(name).extension(), Some("log".as_ref()));
    assert_ne!(log_path(&store, "srv-2")?, first);
    assert_eq!(fs::metadata(store.join("logs"))?.permissions().mode() & 0o777, 0o700);
    Ok(())
}

#[test]
fn the_tail_starts_at_a_whole_character() -> io::Result<()> {
    let (_directory, root) = root()?;
    let path = root.join("build.log");
    fs::write(&path, "abc\u{e9}\u{e9}z")?;
    // The last four bytes cut the first `é` in half.
    assert_eq!(read_tail(&path, 4)?, (8, "\u{e9}z".to_owned()));
    assert_eq!(read_tail(&path, 100)?, (8, "abc\u{e9}\u{e9}z".to_owned()));
    assert_eq!(read_tail(&path, 0)?, (8, String::new()));
    std::os::unix::fs::symlink(&path, root.join("link.log"))?;
    assert!(read_tail(&root.join("link.log"), 4).is_err());
    Ok(())
}
