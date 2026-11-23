//! Shared Lake sequencing for a first-party module with prebuilt dependencies.

use std::io;
/// Maximum number of modules supported in a project build or selected import closure.
pub const MAX_PROJECT_MODULES: usize = 16_384;

/// Check dependency and setup readiness before allowing the selected module to build.
///
/// # Errors
/// Propagates command or post-readiness validation errors.
pub fn run_module(
    name: &str,
    mut run: impl FnMut(&[&str]) -> io::Result<i32>,
    before_build: impl FnOnce() -> io::Result<()>,
) -> io::Result<(i32, Option<i32>)> {
    let mut readiness = run(&["--no-build", "build", &format!("@/+{name}:deps")])?;
    if readiness == 0 {
        // Elaboration and IR generation both widen transitive imports in setup.
        readiness = run(&["--no-build", "query", &format!("@/+{name}:setup")])?;
    }
    if readiness != 0 {
        return Ok((readiness, None));
    }
    before_build()?;
    let built = run(&["--log-level=warning", "build", &format!("@/+{name}:irArts")])?;
    Ok((readiness, Some(built)))
}
