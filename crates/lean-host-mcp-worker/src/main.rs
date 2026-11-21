mod authority;

fn main() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    let deployment = authority::Installed::open()?;
    if let Some(mode) = std::env::args_os().nth(1) {
        if matches!(
            mode.to_str(),
            Some("--lean-rs-ready-host-artifacts" | "--lean-rs-prepare-host-artifacts")
        ) && std::env::args_os().len() == 2
        {
            println!("{}", serde_json::to_string(deployment.prepared())?);
            return Ok(std::process::ExitCode::SUCCESS);
        }
        return Err("unsupported installed worker preparation request".into());
    }
    Ok(lean_rs_worker_child::run_worker_child_with_authority(deployment)?)
}

#[cfg(test)]
mod verification_preflight;
