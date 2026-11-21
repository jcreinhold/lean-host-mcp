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
    SocketAddr::from_pathname(root.join("lean-worker-000000000000000000000000/parent.sock")).map_err(|_| invalid())?;
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
