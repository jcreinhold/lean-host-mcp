//! Location of short-lived SDK sockets. Durable owner records use their existing registry.
use std::{
    fs, io,
    os::unix::{fs::DirBuilderExt as _, fs::MetadataExt as _, net::SocketAddr},
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// The env var that names the socket directory.
pub(crate) const ENV: &str = "LEAN_HOST_MCP_SOCKET_DIR";

/// The directory set by [`use_directory`].
static CHOSEN: OnceLock<PathBuf> = OnceLock::new();

/// Keep this process's worker sockets in `root`, made mode 0700 if absent.
///
/// It is for a process that cannot set `LEAN_HOST_MCP_SOCKET_DIR`, such as a
/// test harness, and it beats that variable. The native-reader helpers this
/// process starts use `root` too. Call it once, before native work; calling it
/// again with the same path changes nothing.
///
/// # Errors
///
/// Fails when [`directory`] would refuse `root`, or when an earlier call chose
/// another directory.
pub fn use_directory(root: &Path) -> io::Result<()> {
    prepare(root)?;
    if *CHOSEN.get_or_init(|| root.to_path_buf()) == root {
        Ok(())
    } else {
        Err(io::Error::other(
            "this process already keeps its sockets in another directory",
        ))
    }
}

/// The private directory that holds worker sockets, made mode 0700 if absent.
///
/// That is the directory set by [`use_directory`], else
/// `LEAN_HOST_MCP_SOCKET_DIR`, else `$XDG_RUNTIME_DIR/lean-host-mcp`, else
/// `~/.lean-host-mcp-ipc`.
///
/// # Errors
///
/// Fails when no directory is configured, when it is not a canonical,
/// user-owned mode-0700 directory outside `/tmp`, or when a worker socket path
/// in it would pass the Unix length limit.
pub fn directory() -> io::Result<PathBuf> {
    let root = if let Some(path) = CHOSEN.get() {
        path.clone()
    } else if let Some(path) = std::env::var_os(ENV) {
        PathBuf::from(path)
    } else if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(path).join("lean-host-mcp")
    } else {
        dirs::home_dir()
            .ok_or_else(|| io::Error::other("set LEAN_HOST_MCP_SOCKET_DIR to a private absolute directory"))?
            .join(".lean-host-mcp-ipc")
    };
    prepare(&root)?;
    Ok(root)
}

/// Keep this test process's worker sockets in `LEAN_HOST_MCP_SOCKET_DIR`,
/// else in `s` in the Cargo directory that holds the test binary. The default
/// is under `HOME`, and a temporary directory makes too long a socket path on
/// macOS.
///
/// That Cargo directory is the binary's nearest ancestor with a
/// `.rustc_info.json`: the target directory, whatever layout Cargo gives the
/// binary under it.
#[cfg(test)]
pub(crate) fn use_test_directory() -> io::Result<()> {
    if std::env::var_os(ENV).is_some() {
        return Ok(());
    }
    let binary = std::env::current_exe()?.canonicalize()?;
    let cargo_dir = binary
        .ancestors()
        .find(|dir| dir.join(".rustc_info.json").is_file())
        .ok_or_else(|| io::Error::other("no Cargo directory holds the test binary; set LEAN_HOST_MCP_SOCKET_DIR"))?;
    use_directory(&cargo_dir.join("s"))
}

const PARENT_SOCKET: &str = "lean-worker-000000000000000000000000/parent.sock";

fn validate_length(root: &Path) -> io::Result<()> {
    SocketAddr::from_pathname(root.join(PARENT_SOCKET))
        .map(|_| ())
        .map_err(|_| io::Error::other("Unix socket path is too long; set LEAN_HOST_MCP_SOCKET_DIR to a shorter private absolute directory outside /tmp"))
}

fn prepare(root: &Path) -> io::Result<()> {
    let invalid = || {
        io::Error::other(
            "LEAN_HOST_MCP_SOCKET_DIR must be a short, canonical, user-owned mode-0700 directory outside /tmp",
        )
    };
    let parent = root.parent().ok_or_else(invalid)?;
    if !root.is_absolute()
        || root.starts_with("/tmp")
        || root.starts_with("/private/tmp")
        || parent.canonicalize()? != parent
    {
        return Err(invalid());
    }
    validate_length(root)?;
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o777 != 0o700
        || root.canonicalize()? != root
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn worker_socket_path_accepts_maximum_and_refuses_one_byte_more() -> io::Result<()> {
        let max_path = if cfg!(target_os = "macos") { 103 } else { 107 };
        let root = |length: usize| PathBuf::from(format!("/{}", "a".repeat(length - PARENT_SOCKET.len() - 2)));
        validate_length(&root(max_path))?;
        let too_long = validate_length(&root(max_path + 1))
            .err()
            .ok_or_else(|| io::Error::other("accepted a socket path over the length limit"))?;
        assert!(
            too_long
                .to_string()
                .contains("set LEAN_HOST_MCP_SOCKET_DIR to a shorter")
        );
        Ok(())
    }
}
