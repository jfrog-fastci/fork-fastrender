use std::io::{self, Read};
use std::path::Path;

/// Backend interface for fetching `file://` resources.
///
/// This exists so sandboxed builds can deny direct filesystem access (or proxy it via IPC) while
/// keeping the `HttpFetcher` logic the same.
pub trait FileBackend: Send + Sync {
  /// Return the file length from metadata if available.
  ///
  /// Implementations may return `Ok(None)` when metadata is unavailable or expensive (in which case
  /// the fetcher falls back to the prefix-probe logic).
  fn metadata_len(&self, path: &Path) -> io::Result<Option<u64>>;

  /// Open a file for reading.
  fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
}

/// Default file backend that reads from the local filesystem via `std::fs`.
#[derive(Debug, Default)]
pub struct StdFsFileBackend;

impl FileBackend for StdFsFileBackend {
  fn metadata_len(&self, path: &Path) -> io::Result<Option<u64>> {
    Ok(Some(std::fs::metadata(path)?.len()))
  }

  fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
    let file = std::fs::File::open(path)?;
    Ok(Box::new(file))
  }
}

/// File backend that rejects all filesystem access.
#[derive(Debug, Default)]
pub struct NoFileBackend;

impl NoFileBackend {
  fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "file backend disabled")
  }
}

impl FileBackend for NoFileBackend {
  fn metadata_len(&self, _path: &Path) -> io::Result<Option<u64>> {
    Err(Self::denied())
  }

  fn open(&self, _path: &Path) -> io::Result<Box<dyn Read + Send>> {
    Err(Self::denied())
  }
}

