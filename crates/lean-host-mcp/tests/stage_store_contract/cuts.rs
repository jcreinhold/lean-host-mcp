// Deterministic process-crash hooks for these disposable tests only.
pub(crate) fn cut(point: &str) {
    if std::env::var("LEAN_STAGE_STORE_CUT").ok().as_deref() != Some(point) {
        return;
    }
    let path = std::env::var_os("LEAN_STAGE_STORE_READY").expect("private helper marker");
    let path = std::path::PathBuf::from(path);
    let pending = path.with_extension("pending");
    std::fs::write(&pending, point).expect("causal cut marker contents");
    std::fs::rename(pending, path).expect("atomic cut marker publication");
    // The test parent owns and kills this exact Child. Expiry is a FAILED helper,
    // never a success path or storage durability observation.
    std::thread::sleep(std::time::Duration::from_secs(8));
    panic!("unreaped disposable crash-cut helper");
}
