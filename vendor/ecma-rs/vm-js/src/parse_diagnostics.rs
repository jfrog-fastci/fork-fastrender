use diagnostics::{Diagnostic, FileId};
use parse_js::error::{SyntaxError, SyntaxErrorType};

const EARLY_ERROR_CODE: &str = "VMJS0004";

// `parse-js` may tweak the wording of this early error; accept both known spellings so
// VM-level diagnostic codes remain stable.
const ARGUMENTS_DISALLOWED_IN_CLASS_INIT_V1: &str =
  "'arguments' is not allowed in class field initializer or static initialization block";
const ARGUMENTS_DISALLOWED_IN_CLASS_INIT_V2: &str =
  "`arguments` is not allowed in class field initializers or static blocks";
const AWAIT_DISALLOWED_IN_STATIC_BLOCK: &str =
  "'await' is not allowed in class static initialization block";

fn parse_js_error_is_vmjs_early_error(typ: SyntaxErrorType) -> bool {
  match typ {
    SyntaxErrorType::ExpectedSyntax(message)
      if message == ARGUMENTS_DISALLOWED_IN_CLASS_INIT_V1
        || message == ARGUMENTS_DISALLOWED_IN_CLASS_INIT_V2
        || message == AWAIT_DISALLOWED_IN_STATIC_BLOCK =>
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
