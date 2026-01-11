//! Registry of diagnostic codes emitted by `native-js`.
//!
//! `native-js` diagnostics use the `NJS####` prefix (see `docs/diagnostic-codes.md`).
//! Keep code definitions centralized here so that:
//! - codes stay stable and non-overlapping
//! - callers can reuse the same code values without duplicating strings

use diagnostics::{sort_diagnostics, sort_labels, Diagnostic, Span};

/// Metadata describing a diagnostic code.
#[derive(Clone, Copy, Debug)]
pub struct Code {
  /// Stable string identifier, e.g. `NJS0200`.
  pub id: &'static str,
  /// Short description of what the diagnostic reports.
  pub description: &'static str,
}

/// NJS0009: Syntax is not supported by the native-js strict compilation subset.
pub const STRICT_SUBSET_UNSUPPORTED_SYNTAX: Code =
  Code::new("NJS0009", "unsupported syntax in native-js strict subset");

/// NJS0010: Type is not supported by the native-js strict compilation subset.
pub const STRICT_SUBSET_UNSUPPORTED_TYPE: Code =
  Code::new("NJS0010", "unsupported type in native-js strict subset");

/// NJS0011: Type cannot be represented in the current native ABI/codegen layer.
pub const UNSUPPORTED_NATIVE_TYPE: Code = Code::new("NJS0011", "unsupported type for native codegen");

/// NJS0200: HIR expression form not supported by the native backend yet.
pub const UNSUPPORTED_EXPR: Code = Code::new("NJS0200", "unsupported expression");

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

/// Sort labels inside each diagnostic and then the diagnostics themselves to
/// keep outputs deterministic regardless of traversal order.
pub fn normalize_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
  for diagnostic in diagnostics.iter_mut() {
    sort_labels(&mut diagnostic.labels);
    diagnostic.notes.sort();
  }
  sort_diagnostics(diagnostics);
}

