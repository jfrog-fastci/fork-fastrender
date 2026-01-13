//! Helper binary for Windows AppContainer sandbox integration tests.
//!
//! This binary is intentionally tiny and dependency-free so it can be spawned inside an
//! AppContainer and validate that:
//! - the process has an accessible current working directory, and
//! - `std::env::temp_dir()` points to a writable location.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

fn main() {
  if let Err(err) = run() {
    eprintln!("{err}");
    std::process::exit(1);
  }
}

fn run() -> Result<(), String> {
  // Validate that the current directory is at least accessible (some libs do relative path I/O,
  // probe the cwd, etc). We don't require that it is writable (e.g. fallback CWD may be System32).
  fs::read_dir(".")
    .map_err(|err| format!("failed to read current directory inside sandbox: {err}"))?
    .next();

  let temp_dir = std::env::temp_dir();
  if temp_dir.as_os_str().is_empty() {
    return Err("std::env::temp_dir() returned empty path".to_string());
  }

  // Create a unique temp file and validate basic read/write/delete operations.
  let file_path = unique_temp_file_path(&temp_dir);
  let payload = b"fastrender-appcontainer-temp-smoke";

  {
    let mut file = fs::File::create(&file_path)
      .map_err(|err| format!("failed to create temp file {}: {err}", file_path.display()))?;
    file
      .write_all(payload)
      .map_err(|err| format!("failed to write temp file {}: {err}", file_path.display()))?;
    file
      .flush()
      .map_err(|err| format!("failed to flush temp file {}: {err}", file_path.display()))?;
  }

  let mut read_back = Vec::new();
  {
    let mut file = fs::File::open(&file_path)
      .map_err(|err| format!("failed to open temp file {}: {err}", file_path.display()))?;
    file
      .read_to_end(&mut read_back)
      .map_err(|err| format!("failed to read temp file {}: {err}", file_path.display()))?;
  }
  if read_back != payload {
    return Err(format!(
      "temp file {} round-trip mismatch (expected {:?}, got {:?})",
      file_path.display(),
      payload,
      read_back
    ));
  }

  fs::remove_file(&file_path)
    .map_err(|err| format!("failed to delete temp file {}: {err}", file_path.display()))?;

  Ok(())
}

fn unique_temp_file_path(temp_dir: &PathBuf) -> PathBuf {
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_nanos();
  temp_dir.join(format!(
    "fastrender_appcontainer_temp_smoke_{}_{}.tmp",
    std::process::id(),
    nanos
  ))
}
