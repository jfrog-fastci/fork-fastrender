use diagnostics::{Diagnostic, FileId};
use parse_js::error::{SyntaxError, SyntaxErrorType};

const EARLY_ERROR_CODE: &str = "VMJS0004";

// `parse-js` may tweak the wording of these early errors. Match on key phrases rather than exact
// strings so VM-level diagnostic codes remain stable.
fn is_arguments_disallowed_in_class_init_message(message: &str) -> bool {
  let message = message.to_ascii_lowercase();
  message.contains("arguments")
    && message.contains("class")
    && (message.contains("field")
      || message.contains("static")
      || message.contains("initializer"))
}

fn is_await_disallowed_in_class_static_block_message(message: &str) -> bool {
  let message = message.to_ascii_lowercase();
  message.contains("await")
    && message.contains("class")
    && message.contains("static")
    && message.contains("block")
}

fn parse_js_error_is_vmjs_early_error(typ: SyntaxErrorType) -> bool {
  match typ {
    // `parse-js` has started surfacing some ECMA-262 early errors as dedicated error variants
    // (instead of generic `ExpectedSyntax(..)` parse errors). Preserve engine-level diagnostic
    // stability by mapping those variants onto VMJS0004 as well.
    SyntaxErrorType::ArgumentsNotAllowedInClassInit => true,
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
