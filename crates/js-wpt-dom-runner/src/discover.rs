use std::path::{Path, PathBuf};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsTestKind {
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

impl JsTestKind {
  pub fn is_runnable_in_window(&self) -> bool {
    matches!(self, JsTestKind::Window | JsTestKind::Any)
  }

  pub fn skip_reason(&self) -> Option<&'static str> {
    match self {
      JsTestKind::Worker => Some("worker tests are not supported yet"),
      JsTestKind::ServiceWorker => Some("service worker tests are not supported yet"),
      JsTestKind::SharedWorker => Some("shared worker tests are not supported yet"),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
  /// The WPT test id, e.g. `dom/nodes/foo.window.js`.
  pub id: String,
  /// Full filesystem path to the test file.
  pub path: PathBuf,
  pub kind: JsTestKind,
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
    let Some(kind) = classify_js_test(path) else {
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

fn classify_js_test(path: &Path) -> Option<JsTestKind> {
  let file_name = path.file_name()?.to_string_lossy();
  if file_name.ends_with(".window.js") {
    return Some(JsTestKind::Window);
  }
  if file_name.ends_with(".any.js") {
    return Some(JsTestKind::Any);
  }
  if file_name.ends_with(".worker.js") || file_name.ends_with(".worker-module.js") {
    return Some(JsTestKind::Worker);
  }
  if file_name.ends_with(".serviceworker.js") || file_name.ends_with(".serviceworker-module.js") {
    return Some(JsTestKind::ServiceWorker);
  }
  if file_name.ends_with(".sharedworker.js") || file_name.ends_with(".sharedworker-module.js") {
    return Some(JsTestKind::SharedWorker);
  }
  None
}
