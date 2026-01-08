use std::path::{Path, PathBuf};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum WptFsError {
  #[error("failed to canonicalize WPT root {0}: {1}")]
  CanonicalizeRoot(PathBuf, #[source] std::io::Error),
  #[error("URL is outside WPT origin: {0}")]
  OutsideOrigin(String),
  #[error("resolved path escapes WPT root: {0}")]
  EscapesRoot(PathBuf),
  #[error("failed to read script {0}: {1}")]
  Read(PathBuf, #[source] std::io::Error),
  #[error("failed to canonicalize path {0}: {1}")]
  CanonicalizePath(PathBuf, #[source] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct WptFs {
  root: PathBuf,
}

impl WptFs {
  pub fn new(root: impl AsRef<Path>) -> Result<Self, WptFsError> {
    let root = root.as_ref().to_path_buf();
    let root = root
      .canonicalize()
      .map_err(|e| WptFsError::CanonicalizeRoot(root.clone(), e))?;
    Ok(Self { root })
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  pub fn read_to_string(&self, path: &Path) -> Result<String, WptFsError> {
    std::fs::read_to_string(path).map_err(|e| WptFsError::Read(path.to_path_buf(), e))
  }

  /// Resolve a WPT URL-like string to a filesystem path under `root`.
  ///
  /// Supports:
  /// - Absolute-path URLs like `/resources/testharness.js`
  /// - Fully-qualified WPT URLs like `https://web-platform.test/resources/testharness.js`
  /// - Relative paths like `support/helper.js` (resolved relative to `base_id_dir`)
  pub fn resolve_url(&self, base_id_dir: &str, url: &str) -> Result<PathBuf, WptFsError> {
    // Absolute within origin.
    if url.starts_with('/') {
      return self.join_under_root(&url[1..]);
    }

    // Fully-qualified.
    if let Ok(parsed) = Url::parse(url) {
      let origin = parsed.origin().unicode_serialization();
      if origin != "https://web-platform.test" && origin != "http://web-platform.test" {
        return Err(WptFsError::OutsideOrigin(url.to_string()));
      }
      let path = parsed.path().trim_start_matches('/');
      return self.join_under_root(path);
    }

    // Relative to the test's directory.
    let rel = if base_id_dir.is_empty() {
      url.to_string()
    } else {
      format!("{base_id_dir}/{url}")
    };
    self.join_under_root(&rel)
  }

  fn join_under_root(&self, rel: &str) -> Result<PathBuf, WptFsError> {
    let candidate = self.root.join(rel);
    let canonical = candidate
      .canonicalize()
      .map_err(|e| WptFsError::CanonicalizePath(candidate.clone(), e))?;
    if !canonical.starts_with(&self.root) {
      return Err(WptFsError::EscapesRoot(canonical));
    }
    Ok(canonical)
  }
}

