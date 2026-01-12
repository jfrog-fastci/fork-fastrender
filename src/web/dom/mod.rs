pub mod selectors;

mod exception;

pub use exception::DomException;

/// HTML document readiness state.
///
/// This is a minimal, spec-shaped subset used for `document.readyState`:
/// `loading` → `interactive` → `complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentReadyState {
  Loading,
  Interactive,
  Complete,
}

impl DocumentReadyState {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Loading => "loading",
      Self::Interactive => "interactive",
      Self::Complete => "complete",
    }
  }
}

/// Page visibility state for `document.visibilityState`.
///
/// This is a minimal subset of the Page Visibility spec; FastRender currently distinguishes only
/// between `visible` and `hidden`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocumentVisibilityState {
  #[default]
  Visible,
  Hidden,
}

impl DocumentVisibilityState {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Visible => "visible",
      Self::Hidden => "hidden",
    }
  }

  pub fn hidden(self) -> bool {
    matches!(self, Self::Hidden)
  }
}
