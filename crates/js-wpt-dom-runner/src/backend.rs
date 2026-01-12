use crate::RunError;
use std::env;

pub use crate::engine::{Backend, BackendInit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
  QuickJs,
  VmJs,
  /// `vm-js` backend executed against a renderer-backed `BrowserDocumentDom2` so layout-sensitive
  /// tests can observe real geometry.
  VmJsRendered,
}

impl BackendKind {
  pub fn as_str(self) -> &'static str {
    match self {
      BackendKind::QuickJs => "quickjs",
      BackendKind::VmJs => "vmjs",
      BackendKind::VmJsRendered => "vmjs-rendered",
    }
  }

  pub fn is_available(self) -> bool {
    match self {
      BackendKind::QuickJs => cfg!(feature = "quickjs"),
      BackendKind::VmJs => {
        #[cfg(feature = "vmjs")]
        {
          crate::backend_vmjs::is_available()
        }
        #[cfg(not(feature = "vmjs"))]
        {
          false
        }
      }
      BackendKind::VmJsRendered => {
        #[cfg(feature = "vmjs")]
        {
          crate::backend_vmjs_rendered::is_available()
        }
        #[cfg(not(feature = "vmjs"))]
        {
          false
        }
      }
    }
  }

  pub fn preferred() -> Self {
    if BackendKind::VmJs.is_available() {
      return BackendKind::VmJs;
    }
    if BackendKind::VmJsRendered.is_available() {
      return BackendKind::VmJsRendered;
    }
    if BackendKind::QuickJs.is_available() {
      return BackendKind::QuickJs;
    }
    BackendKind::QuickJs
  }

  pub fn all_available() -> Vec<Self> {
    let mut out = Vec::new();
    if BackendKind::QuickJs.is_available() {
      out.push(BackendKind::QuickJs);
    }
    if BackendKind::VmJs.is_available() {
      out.push(BackendKind::VmJs);
    }
    if BackendKind::VmJsRendered.is_available() {
      out.push(BackendKind::VmJsRendered);
    }
    out
  }
}

impl std::fmt::Display for BackendKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendSelection {
  /// Choose the best backend available in the current build (prefer `vm-js` when available).
  Auto,
  QuickJs,
  VmJs,
  VmJsRendered,
}

impl BackendSelection {
  pub fn resolve(self) -> BackendKind {
    match self {
      BackendSelection::Auto => BackendKind::preferred(),
      BackendSelection::QuickJs => BackendKind::QuickJs,
      BackendSelection::VmJs => BackendKind::VmJs,
      BackendSelection::VmJsRendered => BackendKind::VmJsRendered,
    }
  }

  /// Reads `FASTERENDER_WPT_DOM_BACKEND` if set.
  ///
  /// Accepted values: `auto` | `quickjs` | `vmjs` | `vmjs-rendered`.
  pub fn from_env() -> Result<Option<Self>, RunError> {
    let Ok(raw) = env::var("FASTERENDER_WPT_DOM_BACKEND") else {
      return Ok(None);
    };
    let value = raw.trim().to_ascii_lowercase();
    let selection = match value.as_str() {
      "" | "auto" => BackendSelection::Auto,
      "quickjs" => BackendSelection::QuickJs,
      "vmjs" => BackendSelection::VmJs,
      "vmjs-rendered" | "vmjs_rendered" | "vmjsrendered" => BackendSelection::VmJsRendered,
      other => {
        return Err(RunError::Js(format!(
          "invalid FASTERENDER_WPT_DOM_BACKEND={other:?} (expected auto|quickjs|vmjs|vmjs-rendered)"
        )))
      }
    };
    Ok(Some(selection))
  }
}

impl Default for BackendSelection {
  fn default() -> Self {
    BackendSelection::Auto
  }
}
