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
    _owner: File,
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
            _owner: owner,
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
                fs::remove_file(&path)?;
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            if records.len() >= capacity || fs::metadata(&path)?.len() > MAX_RECORD_BYTES as u64 {
                return Err(io::Error::other("request journal exceeds configured bounds"));
            }
            let snapshot: Snapshot = serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
            if snapshot.version != 1 || !snapshot.record.is_epoch_bound() || path != journal.path(snapshot.record.id())
            {
                return Err(io::Error::other("unsupported or inconsistent request journal record"));
            }
            records.push(snapshot.record);
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

    pub(crate) fn remove(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => File::open(&self.directory)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
