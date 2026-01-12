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

/// NJS0001: `any` type is forbidden by the native-js strict validator.
pub const STRICT_ANY_TYPE: Code = Code::new("NJS0001", "`any` is not allowed in strict mode");

/// NJS0002: Type assertions are forbidden by the native-js strict validator.
pub const STRICT_TYPE_ASSERTION: Code = Code::new("NJS0002", "type assertions are not allowed in strict mode");

/// NJS0003: Non-null assertions are forbidden by the native-js strict validator.
pub const STRICT_NON_NULL_ASSERTION: Code =
  Code::new("NJS0003", "non-null assertions are not allowed in strict mode");

/// NJS0004: `eval()` is forbidden by the native-js strict validator.
pub const STRICT_EVAL: Code = Code::new("NJS0004", "`eval()` is not allowed in strict mode");

/// NJS0005: `Function()` / `new Function()` is forbidden by the native-js strict validator.
pub const STRICT_FUNCTION_CTOR: Code = Code::new("NJS0005", "`Function` constructor is not allowed in strict mode");

/// NJS0006: `with` statements are forbidden by the native-js strict validator.
pub const STRICT_WITH_STMT: Code = Code::new("NJS0006", "`with` statements are not allowed in strict mode");

/// NJS0007: Computed property access with non-literal keys is forbidden by the native-js strict validator.
pub const STRICT_DYNAMIC_MEMBER: Code =
  Code::new("NJS0007", "computed property access requires literal keys in strict mode");

/// NJS0008: Use of the `arguments` identifier/object is forbidden by the native-js strict validator.
pub const STRICT_ARGUMENTS: Code = Code::new("NJS0008", "`arguments` is not allowed in strict mode");

/// NJS0009: Syntax is not supported by the native-js strict compilation subset.
pub const STRICT_SUBSET_UNSUPPORTED_SYNTAX: Code =
  Code::new("NJS0009", "unsupported syntax in native-js strict subset");

/// NJS0010: Type is not supported by the native-js strict compilation subset.
pub const STRICT_SUBSET_UNSUPPORTED_TYPE: Code =
  Code::new("NJS0010", "unsupported type in native-js strict subset");

/// NJS0011: Type cannot be represented in the current native ABI/codegen layer.
pub const UNSUPPORTED_NATIVE_TYPE: Code = Code::new("NJS0011", "unsupported type for native codegen");

/// NJS0012: Builtin intrinsics are disabled by compiler options.
pub const BUILTINS_DISABLED: Code = Code::new("NJS0012", "builtin intrinsics disabled");

/// NJS0100: Failed to access lowered HIR for the entry file.
pub const HIR_CODEGEN_MISSING_ENTRY_HIR: Code =
  Code::new("NJS0100", "HIR codegen: missing entry file HIR");

/// NJS0101: Failed to access lowered HIR for a function body or locate `main` for codegen.
pub const HIR_CODEGEN_MISSING_FUNCTION_HIR: Code =
  Code::new("NJS0101", "HIR codegen: missing function body HIR");

/// NJS0102: Missing function metadata in lowered HIR.
pub const HIR_CODEGEN_MISSING_FUNCTION_META: Code =
  Code::new("NJS0102", "HIR codegen: missing function metadata");

/// NJS0103: Expression id out of bounds.
pub const HIR_CODEGEN_EXPR_ID_OUT_OF_BOUNDS: Code =
  Code::new("NJS0103", "HIR codegen: expression id out of bounds");

/// NJS0104: Numeric literal cannot be represented as a 32-bit integer.
pub const HIR_CODEGEN_LITERAL_NOT_I32: Code =
  Code::new("NJS0104", "HIR codegen: numeric literal is not a 32-bit integer");

/// NJS0105: Unary operator is not supported by the current HIR codegen subset.
pub const HIR_CODEGEN_UNSUPPORTED_UNARY_OP: Code =
  Code::new("NJS0105", "HIR codegen: unsupported unary operator");

/// NJS0106: Binary operator is not supported by the current HIR codegen subset.
pub const HIR_CODEGEN_UNSUPPORTED_BINARY_OP: Code =
  Code::new("NJS0106", "HIR codegen: unsupported binary operator");

/// NJS0107: Expression form is not supported by the current HIR codegen subset.
pub const HIR_CODEGEN_UNSUPPORTED_EXPR: Code = Code::new("NJS0107", "HIR codegen: unsupported expression");

/// NJS0108: Entry file must export a `main` function.
pub const ENTRYPOINT_MISSING_MAIN_EXPORT: Code =
  Code::new("NJS0108", "entrypoint: entry file must export `main`");

/// NJS0109: Failed to resolve exported `main`.
pub const ENTRYPOINT_UNRESOLVED_MAIN: Code = Code::new("NJS0109", "entrypoint: failed to resolve exported `main`");

/// NJS0110: Exported `main` must be a function with a body.
pub const ENTRYPOINT_MAIN_NOT_FUNCTION: Code = Code::new("NJS0110", "entrypoint: `main` must be a function");

/// NJS0111: Exported `main` must have a supported signature.
pub const ENTRYPOINT_MAIN_BAD_SIGNATURE: Code =
  Code::new("NJS0111", "entrypoint: `main` must have a supported signature");

/// NJS0112: Statement id out of bounds.
pub const HIR_CODEGEN_STMT_ID_OUT_OF_BOUNDS: Code =
  Code::new("NJS0112", "HIR codegen: statement id out of bounds");

/// NJS0113: Statement/variable declaration kind is not supported by the current HIR codegen subset.
pub const HIR_CODEGEN_UNSUPPORTED_STMT: Code = Code::new("NJS0113", "HIR codegen: unsupported statement");

/// NJS0114: Use of unknown/unbound identifier in the current HIR codegen subset.
pub const HIR_CODEGEN_UNKNOWN_IDENTIFIER: Code = Code::new("NJS0114", "HIR codegen: unknown identifier");

/// NJS0115: Not all control-flow paths return a value.
pub const HIR_CODEGEN_MISSING_RETURN: Code = Code::new("NJS0115", "HIR codegen: not all paths return");

/// NJS0116: `return` statement form is not supported in this context.
pub const HIR_CODEGEN_UNSUPPORTED_RETURN: Code = Code::new("NJS0116", "HIR codegen: unsupported return statement");

/// NJS0118: Variable declarations must have an initializer.
pub const HIR_CODEGEN_VAR_DECL_MISSING_INIT: Code =
  Code::new("NJS0118", "HIR codegen: variable declaration missing initializer");

/// NJS0119: Unknown loop label for `break`.
pub const HIR_CODEGEN_UNKNOWN_BREAK_LABEL: Code = Code::new("NJS0119", "HIR codegen: unknown break label");

/// NJS0120: `break` is only supported inside loops.
pub const HIR_CODEGEN_BREAK_OUTSIDE_LOOP: Code = Code::new("NJS0120", "HIR codegen: break outside loop");

/// NJS0121: Unknown loop label for `continue`.
pub const HIR_CODEGEN_UNKNOWN_CONTINUE_LABEL: Code = Code::new("NJS0121", "HIR codegen: unknown continue label");

/// NJS0122: `continue` is only supported inside loops (also used for unsupported binding patterns).
pub const HIR_CODEGEN_INVALID_CONTINUE_OR_BINDING: Code =
  Code::new("NJS0122", "HIR codegen: invalid continue target or binding pattern");

/// NJS0123: Failed to resolve call signature for exported `main`.
pub const HIR_CODEGEN_MAIN_SIGNATURE_MISSING: Code =
  Code::new("NJS0123", "HIR codegen: failed to resolve exported `main` signature");

/// NJS0124: Labels are only supported on loops by the current HIR codegen subset.
pub const HIR_CODEGEN_UNSUPPORTED_LABEL: Code = Code::new("NJS0124", "HIR codegen: unsupported label");

/// NJS0130: Failed to resolve identifier/callee during codegen.
pub const HIR_CODEGEN_FAILED_TO_RESOLVE_IDENT: Code = Code::new("NJS0130", "HIR codegen: failed to resolve ident");

/// NJS0132: Unsupported assignment target.
pub const HIR_CODEGEN_UNSUPPORTED_ASSIGN_TARGET: Code =
  Code::new("NJS0132", "HIR codegen: unsupported assignment target");

/// NJS0134: Unsupported assignment operator.
pub const HIR_CODEGEN_UNSUPPORTED_ASSIGN_OP: Code = Code::new("NJS0134", "HIR codegen: unsupported assignment op");

/// NJS0140: Failed to resolve definition kind for a global/import binding.
pub const HIR_CODEGEN_FAILED_TO_RESOLVE_DEF_KIND: Code =
  Code::new("NJS0140", "HIR codegen: failed to resolve binding kind");

/// NJS0141: Unresolved import binding (or cyclic import resolution).
pub const HIR_CODEGEN_UNRESOLVED_IMPORT_BINDING: Code =
  Code::new("NJS0141", "HIR codegen: unresolved import binding");

/// NJS0142: Unsupported global binding kind in codegen.
pub const HIR_CODEGEN_UNSUPPORTED_GLOBAL_BINDING: Code =
  Code::new("NJS0142", "HIR codegen: unsupported global binding kind");

/// NJS0144: Unsupported call syntax in the current HIR codegen subset.
pub const HIR_CODEGEN_UNSUPPORTED_CALL_SYNTAX: Code = Code::new("NJS0144", "HIR codegen: unsupported call syntax");

/// NJS0145: Call to unknown function (or void call not supported).
pub const HIR_CODEGEN_INVALID_CALL: Code = Code::new("NJS0145", "HIR codegen: invalid call target");

/// NJS0146: Cyclic module dependency detected in runtime module graph.
pub const HIR_CODEGEN_CYCLIC_MODULE_DEPENDENCY: Code =
  Code::new("NJS0146", "HIR codegen: cyclic runtime module dependency");

/// NJS0200: HIR expression form not supported by the native backend yet.
pub const UNSUPPORTED_EXPR: Code = Code::new("NJS0200", "unsupported expression");

/// NJS0201: Failed to access lowered HIR for the entry file.
pub const MISSING_ENTRY_HIR: Code = Code::new("NJS0201", "missing lowered HIR for entry file");

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
