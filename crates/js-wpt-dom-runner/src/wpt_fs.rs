use std::path::{Path, PathBuf};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum WptFsError {
  #[error("failed to canonicalize WPT root {0}: {1}")]
  CanonicalizeRoot(PathBuf, #[source] std::io::Error),
  #[error("WPT root is missing required directory: {0}")]
  MissingRequiredDir(PathBuf),
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
  tests_root: PathBuf,
  resources_root: PathBuf,
}

impl WptFs {
  /// Create a WPT filesystem mapper rooted at `tests/wpt_dom/`.
  ///
  /// Directory layout (see `tests/wpt_dom/README.md`):
  /// - `<root>/tests/` is served as `https://web-platform.test/<path>`
  /// - `<root>/resources/` is served as `https://web-platform.test/resources/<path>`
  pub fn new(root: impl AsRef<Path>) -> Result<Self, WptFsError> {
    let root = root.as_ref().to_path_buf();
    let root = root
      .canonicalize()
      .map_err(|e| WptFsError::CanonicalizeRoot(root.clone(), e))?;
    let tests_root = root.join("tests");
    let resources_root = root.join("resources");
    if !tests_root.is_dir() || !resources_root.is_dir() {
      return Err(WptFsError::MissingRequiredDir(root));
    }
    let tests_root = tests_root
      .canonicalize()
      .map_err(|e| WptFsError::CanonicalizeRoot(tests_root.clone(), e))?;
    let resources_root = resources_root
      .canonicalize()
      .map_err(|e| WptFsError::CanonicalizeRoot(resources_root.clone(), e))?;

    Ok(Self {
      root,
      tests_root,
      resources_root,
    })
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  pub fn tests_root(&self) -> &Path {
    &self.tests_root
  }

  pub fn resources_root(&self) -> &Path {
    &self.resources_root
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
    if url.starts_with("/resources/") {
      let rel = url.trim_start_matches("/resources/");
      return self.join_under(&self.resources_root, rel);
    }
    if url.starts_with('/') {
      return self.join_under(&self.tests_root, &url[1..]);
    }

    // Fully-qualified.
    if let Ok(parsed) = Url::parse(url) {
      let origin = parsed.origin().unicode_serialization();
      if origin != "https://web-platform.test" && origin != "http://web-platform.test" {
        return Err(WptFsError::OutsideOrigin(url.to_string()));
      }
      let path = parsed.path();
      if path.starts_with("/resources/") {
        let rel = path.trim_start_matches("/resources/");
        return self.join_under(&self.resources_root, rel);
      }
      let rel = path.trim_start_matches('/');
      return self.join_under(&self.tests_root, rel);
    }

    // Relative to the test's directory.
    let rel = if base_id_dir.is_empty() {
      url.to_string()
    } else {
      format!("{base_id_dir}/{url}")
    };
    self.join_under(&self.tests_root, &rel)
  }

  fn join_under(&self, base: &Path, rel: &str) -> Result<PathBuf, WptFsError> {
    let candidate = base.join(rel);
    let canonical = candidate
      .canonicalize()
      .map_err(|e| WptFsError::CanonicalizePath(candidate.clone(), e))?;
    if !canonical.starts_with(base) {
      return Err(WptFsError::EscapesRoot(canonical));
    }
    Ok(canonical)
  }
}
