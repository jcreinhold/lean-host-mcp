//! Opt-in integration target for lean-rs-worker-parent with application_launch_input.
#[path = "../../crates/lean-host-mcp-worker/src/verification_preflight.rs"]
mod preflight;

#[test]
fn explicit_approval_reaches_both_worker_generations() -> Result<(), Box<dyn std::error::Error>> {
    use lean_rs_worker_parent::{LeanWorkerChild, LeanWorkerHostHandleBuilder};
    use std::{io, path::PathBuf};

    let fixture = PathBuf::from(std::env::var_os("R2_FRAMED_CHILD").ok_or("missing SDK fixture")?).canonicalize()?;
    let root = std::env::current_dir()?.canonicalize()?;
    preflight::check_handoff(|explicit| {
        let child = LeanWorkerChild::for_toolchain(&fixture, &root)
            .application_launch_input(explicit)
            .map_err(io::Error::other)?
            .checked_native_loader_environment();
        let mut handle = LeanWorkerHostHandleBuilder::shims_only(&root, std::iter::empty::<String>())
            .worker_child(child)
            .open()
            .map_err(io::Error::other)?;
        let result = (|| {
            let first: (u32, String) = serde_json::from_str(&handle.runtime_metadata().worker_version)?;
            handle.restart().map_err(io::Error::other)?;
            let second: (u32, String) = serde_json::from_str(&handle.runtime_metadata().worker_version)?;
            if first.0 == second.0 || first.1 != explicit || first.1 != second.1 {
                return Err(io::Error::other("worker generation or launch input mismatch"));
            }
            Ok(second.1)
        })();
        let closed = handle.try_shutdown().map_err(io::Error::other);
        let received = result?;
        closed?;
        Ok(received)
    })?;
    Ok(())
}
