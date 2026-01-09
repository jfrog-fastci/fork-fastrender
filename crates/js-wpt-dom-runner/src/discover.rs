use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
  /// `*.html` / `*.htm` / `*.xhtml` testharness tests which run in a Window realm.
  Html,
  /// `*.window.js` tests which run in a Window realm.
  Window,
  /// `*.any.js` tests which can run in multiple realms; we currently only run the window variant.
  Any,
  /// `*.worker.js` and related variants (discovered but currently skipped).
  Worker,
  /// `*.serviceworker.js` and related variants (discovered but currently skipped).
  ServiceWorker,
  /// `*.sharedworker.js` and related variants (discovered but currently skipped).
  SharedWorker,
}

impl TestKind {
  pub fn is_runnable_in_window(&self) -> bool {
    matches!(self, TestKind::Html | TestKind::Window | TestKind::Any)
  }

  pub fn skip_reason(&self) -> Option<&'static str> {
    match self {
      TestKind::Worker => Some("worker tests are not supported yet"),
      TestKind::ServiceWorker => Some("service worker tests are not supported yet"),
      TestKind::SharedWorker => Some("shared worker tests are not supported yet"),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
  /// The WPT test id, e.g. `dom/nodes/foo.window.js` or `dom/nodes/foo.html`.
  pub id: String,
  /// Full filesystem path to the test file.
  pub path: PathBuf,
  pub kind: TestKind,
}

impl TestCase {
  pub fn url(&self) -> String {
    format!("https://web-platform.test/{}", self.id)
  }
}

pub fn discover_tests(wpt_root: impl AsRef<Path>) -> Result<Vec<TestCase>, DiscoverError> {
  let wpt_root = wpt_root.as_ref().to_path_buf();
  let root = wpt_root
    .canonicalize()
    .map_err(|e| DiscoverError::Canonicalize(wpt_root.clone(), e))?;

  let mut out = Vec::new();
  for entry in WalkDir::new(&root) {
    let entry = entry.map_err(DiscoverError::Walk)?;
    if !entry.file_type().is_file() {
      continue;
    }
    let path = entry.path();
    let Some(kind) = classify_test(path) else {
      continue;
    };
    let rel = path.strip_prefix(&root).unwrap_or(path);
    let id = rel
      .components()
      .map(|c| c.as_os_str().to_string_lossy())
      .collect::<Vec<_>>()
      .join("/");
    out.push(TestCase {
      id,
      path: path.to_path_buf(),
      kind,
    });
  }
  out.sort_by(|a, b| a.id.cmp(&b.id));
  Ok(out)
}

#[derive(Debug, Error)]
pub enum DiscoverError {
  #[error("failed to canonicalize WPT root {0}: {1}")]
  Canonicalize(PathBuf, #[source] std::io::Error),
  #[error("walkdir error: {0}")]
  Walk(#[from] walkdir::Error),
}

fn classify_test(path: &Path) -> Option<TestKind> {
  let file_name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
  if file_name.ends_with(".html") || file_name.ends_with(".htm") || file_name.ends_with(".xhtml") {
    return Some(TestKind::Html);
  }
  if file_name.ends_with(".window.js") {
    return Some(TestKind::Window);
  }
  if file_name.ends_with(".any.js") {
    return Some(TestKind::Any);
  }
  if file_name.ends_with(".worker.js") || file_name.ends_with(".worker-module.js") {
    return Some(TestKind::Worker);
  }
  if file_name.ends_with(".serviceworker.js") || file_name.ends_with(".serviceworker-module.js") {
    return Some(TestKind::ServiceWorker);
  }
  if file_name.ends_with(".sharedworker.js") || file_name.ends_with(".sharedworker-module.js") {
    return Some(TestKind::SharedWorker);
  }
  None
}
