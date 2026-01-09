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
