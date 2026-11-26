//! The `[build]` and `[recipes.<name>]` tables of a project's config files.
//!
//! `[build]` names the targets that guard every build, the time a build may
//! take, and how much log it keeps. Each `[recipes.<name>]` table names Lake
//! commands a client may run by name. The server reads both tables on every
//! build and refuses the build when a key is unknown or a value is wrong,
//! naming every problem, so a typo never drops a recipe or a guard.
//!
//! Steps run in the Lake root with the server's environment, so a variable
//! such as `LEAN_NUM_THREADS` belongs where the server starts.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::lake_args::{check_step, has_command};

/// Default `[build] timeout_secs`: four hours.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 14_400;
/// Default `[build] log_limit_mib`.
pub(crate) const DEFAULT_LOG_LIMIT_MIB: u64 = 16;

/// The `[build]` table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildSettings {
    /// Targets that `lake build --no-build` must find up to date.
    #[serde(default)]
    pub(crate) guard: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    pub(crate) timeout_secs: u64,
    #[serde(default = "default_log_limit_mib")]
    pub(crate) log_limit_mib: u64,
}

impl Default for BuildSettings {
    fn default() -> Self {
        Self {
            guard: Vec::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            log_limit_mib: DEFAULT_LOG_LIMIT_MIB,
        }
    }
}

const fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

const fn default_log_limit_mib() -> u64 {
    DEFAULT_LOG_LIMIT_MIB
}

/// One `[recipes.<name>]` table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipe {
    /// The words after `lake` of each run, in order. The first failing run
    /// ends the recipe.
    pub(crate) steps: Vec<Vec<String>>,
    #[serde(default)]
    pub(crate) append: Append,
    #[serde(default)]
    pub(crate) guard: GuardPlace,
    /// The deadline of this recipe's builds, in place of `[build] timeout_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeout_secs: Option<u64>,
}

/// Which steps of a recipe get the client's `args`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Append {
    /// None: the recipe refuses `args`.
    #[default]
    None,
    /// The last step.
    Last,
    /// Every step.
    Each,
}

/// When a recipe runs the `[build]` guard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GuardPlace {
    /// Before the steps. A failing guard ends the recipe.
    #[default]
    Before,
    /// After the steps.
    After,
    /// Never.
    None,
}

/// The build tables of one project, checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BuildConfig {
    pub(crate) build: BuildSettings,
    pub(crate) recipes: BTreeMap<String, Recipe>,
}

impl BuildConfig {
    /// Read and check the build tables of Lake root `root`.
    ///
    /// # Errors
    /// Names every unreadable file, unknown key and bad value.
    pub(crate) fn load(root: &std::path::Path) -> Result<Self, Vec<String>> {
        let table = crate::config_file::load_strict(root).map_err(|error| vec![error])?;
        Self::from_table(&table)
    }

    /// Check the build tables of a merged config table.
    ///
    /// # Errors
    /// Names every unknown key and bad value.
    pub(crate) fn from_table(table: &toml::Table) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let build = table
            .get("build")
            .and_then(|value| section::<BuildSettings>(value, "[build]", &mut errors))
            .unwrap_or_default();
        check_build(&build, &mut errors);
        let mut recipes = BTreeMap::new();
        match table.get("recipes").map(toml::Value::as_table) {
            None => {}
            Some(None) => errors.push("`recipes` must be a table of recipe tables".to_owned()),
            Some(Some(tables)) => {
                for (name, value) in tables {
                    let at = format!("[recipes.{name}]");
                    if let Some(recipe) = section::<Recipe>(value, &at, &mut errors)
                        && check_recipe(&at, &recipe, &mut errors)
                    {
                        recipes.insert(name.clone(), recipe);
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(Self { build, recipes })
        } else {
            Err(errors)
        }
    }

    /// The checked tables as TOML, for `config check`.
    ///
    /// # Errors
    /// Returns the serializer's error.
    pub(crate) fn render(&self) -> Result<String, toml::ser::Error> {
        toml::to_string(self)
    }
}

fn section<T: DeserializeOwned>(value: &toml::Value, at: &str, errors: &mut Vec<String>) -> Option<T> {
    match value.clone().try_into() {
        Ok(section) => Some(section),
        Err(error) => {
            errors.push(format!("{at}: {}", error.message()));
            None
        }
    }
}

fn check_build(build: &BuildSettings, errors: &mut Vec<String>) {
    let at = "[build]";
    for target in &build.guard {
        if target.is_empty() || target.contains('\0') || target.starts_with('-') {
            errors.push(format!(
                "{at}: guard target `{}` must be a Lake target, not empty and not starting with `-`",
                target.escape_default()
            ));
        }
    }
    if build.timeout_secs == 0 {
        errors.push(format!("{at}: `timeout_secs` must be at least 1"));
    }
    if build.log_limit_mib == 0 || build.log_limit_mib.checked_mul(1 << 20).is_none() {
        errors.push(format!("{at}: `log_limit_mib` must be at least 1 and fit in bytes"));
    }
}

/// Whether `recipe` passes its checks. Names each problem in `errors`.
fn check_recipe(at: &str, recipe: &Recipe, errors: &mut Vec<String>) -> bool {
    let before = errors.len();
    if recipe.steps.is_empty() {
        errors.push(format!("{at}: `steps` is empty"));
    }
    if recipe.timeout_secs == Some(0) {
        errors.push(format!("{at}: `timeout_secs` must be at least 1"));
    }
    for (index, step) in recipe.steps.iter().enumerate() {
        match check_step(step, step.len()) {
            Err(problem) => errors.push(format!("{at}: step {index}: {problem}")),
            Ok(_) if !has_command(step) => errors.push(format!("{at}: step {index} names no Lake command")),
            Ok(_) => {}
        }
    }
    errors.len() == before
}
