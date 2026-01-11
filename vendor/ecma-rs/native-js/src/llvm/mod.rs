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

pub mod gc;
pub mod passes;
pub mod statepoint_directives;

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
