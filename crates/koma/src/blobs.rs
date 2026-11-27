//! Content-addressed blob store under the data directory: pages that could
//! not be served from their archive in place (compressed ones) are written
//! here once, named by their BLAKE3 hash.
//!
//! `blobs/ab/abcdef….png`: two-character fan-out keeps directories small.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("blobs"),
        }
    }

    pub fn path(&self, hash: &blake3::Hash, format: &str) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(format!("{hex}.{format}"))
    }

    /// A temporary file in the store, renamed into place by [`BlobStore::commit`].
    pub fn temp(&self) -> io::Result<(PathBuf, File)> {
        let dir = self.root.join("tmp");
        fs::create_dir_all(&dir)?;
        for attempt in 0u32.. {
            let path = dir.join(format!("{}-{attempt}", std::process::id()));
            match File::options().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!("u32 attempts exhausted")
    }

    /// Moves a finished temp file to its content address. If the blob is
    /// already there (same bytes), the temp file is dropped instead.
    pub fn commit(&self, temp: &Path, hash: &blake3::Hash, format: &str) -> io::Result<PathBuf> {
        let path = self.path(hash, format);
        if path.exists() {
            fs::remove_file(temp)?;
        } else {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::rename(temp, &path)?;
        }
        Ok(path)
    }

    /// Removes leftovers of an interrupted scan.
    pub fn clean_temp(&self) -> io::Result<()> {
        match fs::remove_dir_all(self.root.join("tmp")) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}
