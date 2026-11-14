//! Per-request atomic snapshots. This journal is evidence, not a replay queue.
//! One broker owns a directory; its OS file lock is released on process death.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    record: StoredRequest,
}

use crate::tasks::{MAX_RECORD_BYTES, StoredRequest};

#[derive(Debug)]
pub(crate) struct RequestJournal {
    directory: PathBuf,
    owner: File,
}

impl RequestJournal {
    pub(crate) fn open(directory: &Path, capacity: usize) -> io::Result<(Self, Vec<StoredRequest>)> {
        fs::create_dir_all(directory)?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("owner.lock"))?;
        owner.try_lock().map_err(io::Error::other)?;
        let journal = Self {
            directory: directory.to_path_buf(),
            owner,
        };
        let mut records = Vec::new();
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("request-write-"))
            {
                // Only our own incomplete atomic write, after taking ownership.
                if let Err(error) = fs::remove_file(&path) {
                    tracing::warn!(%error, path = %path.display(), "could not remove incomplete journal write; ignoring it");
                }
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let read = || -> io::Result<StoredRequest> {
                if fs::metadata(&path)?.len() > MAX_RECORD_BYTES as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "request record exceeds size bound",
                    ));
                }
                let snapshot: Snapshot = serde_json::from_slice(&fs::read(&path)?)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if snapshot.version != 1
                    || !snapshot.record.is_epoch_bound()
                    || path != journal.path(snapshot.record.id())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsupported or inconsistent request journal record",
                    ));
                }
                Ok(snapshot.record)
            };
            let record = match read() {
                Ok(record) => record,
                Err(error) => {
                    // The new broker has a fresh epoch: even an unreadable ID
                    // cannot be retried as new work. Preserve the damaged bytes.
                    tracing::warn!(%error, path = %path.display(), "request record invalid or unreadable; preserving it separately");
                    let mut quarantine = path.with_extension("json.quarantined");
                    if fs::symlink_metadata(&quarantine).is_ok() {
                        quarantine = path.with_extension(format!("json.{}.quarantined", uuid::Uuid::new_v4()));
                    }
                    fs::rename(&path, &quarantine).map_err(|error| {
                        io::Error::new(error.kind(), format!(
                            "could not preserve unreadable request record {} separately: {error}; no requests were admitted",
                            path.display()
                        ))
                    })?;
                    continue;
                }
            };
            if records.len() >= capacity {
                return Err(io::Error::other("request journal exceeds configured capacity"));
            }
            records.push(record);
        }
        Ok((journal, records))
    }

    fn path(&self, id: &str) -> PathBuf {
        let mut name = String::with_capacity(69);
        for byte in Sha256::digest(id.as_bytes()) {
            let _ = write!(name, "{byte:02x}");
        }
        name.push_str(".json");
        self.directory.join(name)
    }

    pub(crate) fn write(&self, id: &str, record: &StoredRequest) -> io::Result<()> {
        let bytes = serde_json::to_vec(&Snapshot {
            version: 1,
            record: record.clone(),
        })
        .map_err(io::Error::other)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(io::Error::other("request record exceeds size bound"));
        }
        let mut temp = tempfile::Builder::new()
            .prefix("request-write-")
            .tempfile_in(&self.directory)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(self.path(id)).map_err(io::Error::other)?;
        File::open(&self.directory)?.sync_all()
    }

    pub(crate) fn check_writable(&self) -> io::Result<()> {
        let probe = tempfile::Builder::new()
            .prefix("request-write-")
            .tempfile_in(&self.directory)?;
        probe.as_file().sync_all()?;
        probe.close()?;
        File::open(&self.directory)?.sync_all()
    }

    pub(crate) fn remove(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => File::open(&self.directory)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for RequestJournal {
    fn drop(&mut self) {
        // A concurrently spawned child can briefly inherit this open file
        // description before exec closes it. Closing only our descriptor would
        // leave its lock held until that child execs or exits.
        if let Err(error) = self.owner.unlock() {
            tracing::warn!(%error, "could not unlock request journal");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert the journal owner lifetime")]
mod tests {
    use super::*;

    #[test]
    fn closing_owner_unlocks_even_while_a_duplicate_descriptor_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, _) = RequestJournal::open(dir.path(), 128).unwrap();
        let inherited = journal.owner.try_clone().unwrap();
        assert!(RequestJournal::open(dir.path(), 128).is_err());
        drop(journal);
        let (next, _) = RequestJournal::open(dir.path(), 128).unwrap();
        drop(inherited);
        assert!(RequestJournal::open(dir.path(), 128).is_err());
        drop(next);
        assert!(RequestJournal::open(dir.path(), 128).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn unreadable_record_is_preserved_separately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unreadable.json");
        std::os::unix::fs::symlink(dir.path().join("temporarily-missing"), &path).unwrap();
        let (_, records) = RequestJournal::open(dir.path(), 128).unwrap();
        assert!(records.is_empty());
        assert!(std::fs::symlink_metadata(&path).is_err());
        assert!(std::fs::symlink_metadata(path.with_extension("json.quarantined")).is_ok());
        // A dangling archived symlink still occupies its filename and must not
        // be overwritten by a later unreadable record with the same source path.
        std::os::unix::fs::symlink(dir.path().join("another-missing"), &path).unwrap();
        RequestJournal::open(dir.path(), 128).unwrap();
        assert_eq!(
            std::fs::read_link(path.with_extension("json.quarantined")).unwrap(),
            dir.path().join("temporarily-missing")
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|entry| entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "quarantined"))
                .count(),
            2
        );
    }
}
