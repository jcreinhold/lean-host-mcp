//! Location of short-lived SDK sockets. Durable owner records use their existing registry.
use std::{
    fs, io,
    os::unix::{fs::DirBuilderExt as _, fs::MetadataExt as _, net::SocketAddr},
    path::{Path, PathBuf},
};

pub(crate) fn directory() -> io::Result<PathBuf> {
    let root = if let Some(path) = std::env::var_os("LEAN_HOST_MCP_SOCKET_DIR") {
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
