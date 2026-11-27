mod authority;

/// The SDK flag that prepares the shims. With a directory after it, the
/// installer runs it before this generation has a record: the SDK writes the
/// shim sources this worker was built with to that new directory and builds
/// them there.
const PREPARE_FLAG: &str = "--lean-rs-prepare-host-artifacts";

fn main() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    if std::env::args_os().len() == 3 && std::env::args_os().nth(1).is_some_and(|mode| mode == PREPARE_FLAG) {
        return Ok(lean_rs_worker_child::run_worker_child_stdio());
    }
    let deployment = authority::Installed::open()?;
    if let Some(mode) = std::env::args_os().nth(1) {
        if matches!(mode.to_str(), Some("--lean-rs-ready-host-artifacts" | PREPARE_FLAG))
            && std::env::args_os().len() == 2
        {
            println!("{}", serde_json::to_string(deployment.ready()?)?);
            return Ok(std::process::ExitCode::SUCCESS);
        }
        return Err("unsupported installed worker preparation request".into());
    }
    Ok(lean_rs_worker_child::run_worker_child_with_authority(deployment)?)
}
