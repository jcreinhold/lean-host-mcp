//! Restricted observers must not report a live server as dead or recommend recovery.
#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn doctor_keeps_live_record_when_external_process_tools_are_unavailable() {
    let dir = tempfile::tempdir().expect("temp registry");
    let tools = dir.path().join("tools");
    fs::create_dir(&tools).expect("tool directory");
    for name in ["ps", "kill"] {
        let path = tools.join(name);
        fs::write(&path, "#!/bin/sh\necho 'Operation not permitted' >&2\nexit 1\n").expect("fake observer");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("executable observer");
    }
    let pid = std::process::id();
    let record = dir.path().join(format!("{pid}.json"));
    fs::write(
        &record,
        serde_json::to_vec(&serde_json::json!({
            "pid": pid,
            "executable": std::env::current_exe().expect("current exe"),
            "cwd": dir.path(),
            "started_unix_millis": 1,
            "parent_pid_at_start": 50,
            "process_group_id": null,
            "transport": "stdio",
            "bind": null,
            "http_path": null
        }))
        .expect("serialize record"),
    )
    .expect("write record");
    let output = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
        .args(["doctor", "processes", "--cleanup-stale-records"])
        .env("LEAN_HOST_MCP_PROCESS_REGISTRY_DIR", dir.path())
        .env("PATH", &tools)
        .output()
        .expect("run doctor");
    assert!(output.status.success(), "doctor failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("doctor stdout");
    assert!(stdout.contains(&format!("pid={pid} alive=true")), "{stdout}");
    assert!(stdout.contains("current_parent_pid=-"), "{stdout}");
    assert!(stdout.contains("executable_match=unknown"), "{stdout}");
    assert!(stdout.contains("child_pids=unknown"), "{stdout}");
    assert!(stdout.contains("stale_client=false"), "{stdout}");
    assert!(!stdout.contains("exact_terminate="), "{stdout}");
    assert!(!stdout.contains("stale_record_cleanup="), "{stdout}");
    assert!(record.exists(), "live record was removed");
}
