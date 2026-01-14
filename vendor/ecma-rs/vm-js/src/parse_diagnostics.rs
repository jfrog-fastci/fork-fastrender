use diagnostics::{Diagnostic, FileId};
use parse_js::error::{SyntaxError, SyntaxErrorType};

const EARLY_ERROR_CODE: &str = "VMJS0004";

// `parse-js` may tweak the wording of these early errors. Match on key phrases rather than exact
// strings so VM-level diagnostic codes remain stable.
fn is_arguments_disallowed_in_class_init_message(message: &str) -> bool {
  message.contains("arguments")
    && message.contains("not allowed")
    && (message.contains("class field") || message.contains("static"))
}

fn is_await_disallowed_in_class_static_block_message(message: &str) -> bool {
  message.contains("await")
    && message.contains("not allowed")
    && message.contains("class")
    && message.contains("static")
}

fn parse_js_error_is_vmjs_early_error(typ: SyntaxErrorType) -> bool {
  match typ {
    SyntaxErrorType::ExpectedSyntax(message)
      if is_arguments_disallowed_in_class_init_message(message)
        || is_await_disallowed_in_class_static_block_message(message) =>
    {
      true
    }
    _ => false,
  }
}

/// Convert a `parse-js` syntax error into a shared `diagnostics::Diagnostic`.
///
/// `vm-js` uses a single stable diagnostic code (`VMJS0004`) for many ECMA-262 early errors.
/// `parse-js` surfaces some of those conditions as parse errors with `PS*` codes, so we map the
/// corresponding diagnostics onto `VMJS0004` for engine-level consistency.
pub(crate) fn parse_js_error_to_diagnostic(err: &SyntaxError, file: FileId) -> Diagnostic {
  let mut diag = err.to_diagnostic(file);
  if parse_js_error_is_vmjs_early_error(err.typ) {
    diag.code = EARLY_ERROR_CODE.into();
  }
  diag
}
