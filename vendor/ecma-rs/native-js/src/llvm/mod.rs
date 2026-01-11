//! LLVM integration helpers for `native-js`.
//!
//! The native backend uses LLVM's *statepoint* infrastructure to support a
//! moving GC. On LLVM 18, the "manual" path (constructing `gc.statepoint`
//! intrinsics directly) is easy to get wrong:
//!
//! - Intrinsic signatures contain `immarg` parameters and require the callee
//!   argument to be annotated with `elementtype(<fn-ty>)`.
//! - Manually-built statepoints require extra trailing `i32 0, i32 0`
//!   transition/flags fields.
//!
//! Instead of computing liveness and constructing statepoints in Rust, we rely
//! on LLVM's `rewrite-statepoints-for-gc` pass to:
//! - rewrite plain calls into `llvm.experimental.gc.statepoint.*`
//! - attach the required `"gc-live"` operand bundle
//! - insert `llvm.experimental.gc.relocate.*` / `gc.result.*` and rewrite uses
 
use inkwell::module::Module;
use inkwell::targets::{FileType, TargetMachine};
use llvm_sys::prelude::LLVMModuleRef;

pub mod gc;
pub mod gc_lint;
pub mod passes;
pub mod statepoints;
pub mod statepoint_directives;

pub use gc_lint::{lint_gc_pointer_discipline, LintError, LintRule, LintViolation};

/// Run the GC pointer discipline lint in debug builds/tests.
///
/// In release builds this is a no-op unless the `gc-lint` feature is enabled.
#[cfg(any(debug_assertions, feature = "gc-lint"))]
pub fn debug_lint_gc_pointer_discipline(module: LLVMModuleRef) -> Result<(), LintError> {
  lint_gc_pointer_discipline(module)
}

/// Release builds omit GC IR lint by default for compile-time performance.
#[cfg(not(any(debug_assertions, feature = "gc-lint")))]
pub fn debug_lint_gc_pointer_discipline(_module: LLVMModuleRef) -> Result<(), LintError> {
  Ok(())
}

/// Apply the target triple + data layout from `target_machine` onto `module`.
///
/// When linking LLVM bitcode with `clang -flto`, missing or mismatched target
/// information produces warnings and can lead to incorrect codegen.
pub fn apply_target_machine(module: &Module<'_>, target_machine: &TargetMachine) {
  module.set_triple(&target_machine.get_triple());
  module.set_data_layout(&target_machine.get_target_data().get_data_layout());
}

/// Emit LLVM bitcode into memory.
pub fn emit_bitcode(module: &Module<'_>, target_machine: &TargetMachine) -> Vec<u8> {
  apply_target_machine(module, target_machine);
  module.write_bitcode_to_memory().as_slice().to_vec()
}

/// Emit an object file into memory.
pub fn emit_object(module: &Module<'_>, target_machine: &TargetMachine) -> Vec<u8> {
  apply_target_machine(module, target_machine);
  target_machine
    .write_to_memory_buffer(module, FileType::Object)
    .expect("write object")
    .as_slice()
    .to_vec()
}
