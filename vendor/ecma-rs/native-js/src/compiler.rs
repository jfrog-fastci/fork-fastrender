//! HIR/typecheck-driven compilation entry points.
//!
//! The native backend's eventual pipeline is:
//!
//! `parse-js` → `hir-js` → `typecheck-ts` → `native-js` lowering → LLVM IR
//!
//! The LLVM IR lowering is still under construction, but we can already run the
//! strict-subset validator on a fully typechecked program.

use diagnostics::Diagnostic;
use typecheck_ts::{FileKey, Host, Program};

use crate::validate::validate_strict_subset;

/// Parse + typecheck a TypeScript program and then validate that it fits the
/// strict native-js subset.
///
/// This helper is intended for the native compilation driver: validation runs
/// only after the program typechecks cleanly, and must complete before any LLVM
/// IR is generated.
pub fn typecheck_and_validate_strict_subset(
  host: impl Host,
  roots: Vec<FileKey>,
) -> Result<Program, Vec<Diagnostic>> {
  let program = Program::new(host, roots);
  let diagnostics = program.check();
  if !diagnostics.is_empty() {
    return Err(diagnostics);
  }
  validate_strict_subset(&program)?;
  Ok(program)
}

