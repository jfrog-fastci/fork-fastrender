//! Registry of diagnostic codes emitted by `native-js`.
//!
//! `native-js` diagnostics use the `NJS####` prefix (see `docs/diagnostic-codes.md`).
//! Keep code definitions centralized here so that:
//! - codes stay stable and non-overlapping
//! - callers can reuse the same code values without duplicating strings

use diagnostics::{Diagnostic, Span};

/// Metadata describing a diagnostic code.
#[derive(Clone, Copy, Debug)]
pub struct Code {
  /// Stable string identifier, e.g. `NJS0001`.
  pub id: &'static str,
  /// Short description of what the diagnostic reports.
  pub description: &'static str,
}

impl Code {
  pub const fn new(id: &'static str, description: &'static str) -> Self {
    Self { id, description }
  }

  pub const fn as_str(&self) -> &'static str {
    self.id
  }

  pub fn error(&self, message: impl Into<String>, primary: Span) -> Diagnostic {
    Diagnostic::error(self.id, message, primary)
  }

  pub fn warning(&self, message: impl Into<String>, primary: Span) -> Diagnostic {
    Diagnostic::warning(self.id, message, primary)
  }
}
