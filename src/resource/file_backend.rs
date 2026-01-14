use std::io::{self, Read, Seek};
use std::path::Path;

/// Trait object helper for a file handle that supports streaming reads and seeks.
///
/// Rust trait objects can only have a single non-auto "principal" trait, so we bundle `Read + Seek`
/// into a named trait and add `Send` as a supertrait.
pub trait FileRead: Read + Seek + Send {}

impl<T: Read + Seek + Send> FileRead for T {}

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
  fn open(&self, path: &Path) -> io::Result<Box<dyn FileRead>>;
}

/// Default file backend that reads from the local filesystem via `std::fs`.
#[derive(Debug, Default)]
pub struct StdFsFileBackend;

impl FileBackend for StdFsFileBackend {
  fn metadata_len(&self, path: &Path) -> io::Result<Option<u64>> {
    Ok(Some(std::fs::metadata(path)?.len()))
  }

  fn open(&self, path: &Path) -> io::Result<Box<dyn FileRead>> {
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

  fn open(&self, _path: &Path) -> io::Result<Box<dyn FileRead>> {
    Err(Self::denied())
  }
}
