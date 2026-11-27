//! SHA-256 digests as lowercase hex, the form every receipt, stamp and
//! fingerprint in this crate records.

use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::path::Path;

use sha2::{Digest as _, Sha256};

/// The lowercase hex of `bytes`, two characters a byte.
pub(crate) fn hex(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut text = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The SHA-256 of `bytes`, as lowercase hex.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

/// The SHA-256 of the file at `path`, read in chunks, as lowercase hex.
///
/// # Errors
///
/// Returns the error from opening or reading the file.
pub(crate) fn file_sha256_hex(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        let chunk = buffer
            .get(..count)
            .ok_or_else(|| io::Error::other("read past its buffer"))?;
        if chunk.is_empty() {
            return Ok(hex(hasher.finalize()));
        }
        hasher.update(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_and_its_bytes_have_the_same_digest() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("data");
        let bytes: Vec<u8> = (0..20_000_u32).flat_map(u32::to_le_bytes).collect();
        std::fs::write(&path, &bytes)?;
        assert_eq!(file_sha256_hex(&path)?, sha256_hex(&bytes));
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        Ok(())
    }
}
