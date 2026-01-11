use inkwell::module::Module;
use inkwell::targets::TargetMachine;
use llvm_sys::core::{
  LLVMAddFunction, LLVMCountParamTypes, LLVMFunctionType, LLVMGetModuleContext, LLVMGetNamedFunction,
  LLVMGetReturnType, LLVMGetTypeKind, LLVMGlobalGetValueType, LLVMIsFunctionVarArg, LLVMVoidTypeInContext,
};
use llvm_sys::error::{LLVMDisposeErrorMessage, LLVMGetErrorMessage};
use llvm_sys::transforms::pass_builder::{
  LLVMCreatePassBuilderOptions, LLVMDisposePassBuilderOptions, LLVMRunPasses,
};
use std::ffi::{CStr, CString};
use std::ptr;

#[derive(Debug, thiserror::Error)]
pub enum PassError {
  #[error(transparent)]
  GcLint(#[from] super::LintError),
  #[error("LLVMRunPasses failed for pipeline `{pipeline}`: {message}")]
  RunPasses { pipeline: String, message: String },
  #[error("LLVM module defines `{name}` with incompatible signature (expected `void ()`)")]
  IncompatibleSafepointPollSignature { name: String },
}

/// Ensure the module contains the `gc.safepoint_poll` declaration that LLVM's
/// `place-safepoints` pass expects to exist.
///
/// On Ubuntu LLVM 18.1.3, `place-safepoints` can segfault when it tries to
/// materialize this declaration itself. Predeclaring it avoids that crash.
pub fn ensure_gc_safepoint_poll_decl(module: &Module<'_>) -> Result<(), PassError> {
  // Hardcoded name used by LLVM's statepoint safepointing scheme.
  let name = CString::new("gc.safepoint_poll").expect("gc.safepoint_poll contains NUL");

  unsafe {
    let existing = LLVMGetNamedFunction(module.as_mut_ptr(), name.as_ptr());
    if !existing.is_null() {
      // Ensure the signature matches `declare void @gc.safepoint_poll()`.
      //
      // With opaque pointers, `LLVMTypeOf(existing)` may be just `ptr`, so use
      // `LLVMGlobalGetValueType` instead of trying to peel pointer element types.
      let fn_ty = LLVMGlobalGetValueType(existing);
      let ret_ty = LLVMGetReturnType(fn_ty);
      let param_count = LLVMCountParamTypes(fn_ty);
      let varargs = LLVMIsFunctionVarArg(fn_ty) != 0;

      if LLVMGetTypeKind(ret_ty) != llvm_sys::LLVMTypeKind::LLVMVoidTypeKind
        || param_count != 0
        || varargs
      {
        return Err(PassError::IncompatibleSafepointPollSignature {
          name: "gc.safepoint_poll".to_string(),
        });
      }

      return Ok(());
    }

    let ctx = LLVMGetModuleContext(module.as_mut_ptr());
    let void_ty = LLVMVoidTypeInContext(ctx);
    let fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
    LLVMAddFunction(module.as_mut_ptr(), name.as_ptr(), fn_ty);
  }

  Ok(())
}

/// Runs LLVM's `rewrite-statepoints-for-gc` pass on `module`.
///
/// This rewrites normal calls into `llvm.experimental.gc.statepoint.*` and
/// materializes `llvm.experimental.gc.relocate.*` for GC-managed pointers that
/// are live across each safepoint.
///
/// In debug builds we also run `verify<safepoint-ir>` to catch invalid safepoint
/// IR early.
pub fn rewrite_statepoints_for_gc(module: &Module<'_>, target_machine: &TargetMachine) -> Result<(), PassError> {
  super::debug_lint_module_gc_pointer_discipline(module)?;

  let pipeline = if cfg!(debug_assertions) {
    "rewrite-statepoints-for-gc,verify<safepoint-ir>"
  } else {
    "rewrite-statepoints-for-gc"
  };

  run_pass_pipeline(module, target_machine, pipeline)?;

  super::debug_lint_module_gc_pointer_discipline(module)?;
  Ok(())
}

/// Runs `place-safepoints` + `rewrite-statepoints-for-gc` on `module`.
///
/// This is the "easy correctness" path for making GC-managed functions safe:
/// - `place-safepoints` inserts poll calls (`@gc.safepoint_poll`) at entry and
///   loop backedges (for unknown-trip loops).
/// - `rewrite-statepoints-for-gc` converts those calls (and any other calls) into
///   `llvm.experimental.gc.statepoint.*` so stack maps/relocations are emitted.
///
/// In debug builds we also run `verify<safepoint-ir>` to catch invalid safepoint
/// IR early.
///
/// On LLVM 18.1.3, `place-safepoints` can segfault unless the module already
/// declares `declare void @gc.safepoint_poll()`. This helper applies that
/// workaround before running the pass pipeline.
pub fn place_safepoints_and_rewrite_statepoints_for_gc(
  module: &Module<'_>,
  target_machine: &TargetMachine,
) -> Result<(), PassError> {
  super::debug_lint_module_gc_pointer_discipline(module)?;

  ensure_gc_safepoint_poll_decl(module)?;

  let pipeline = if cfg!(debug_assertions) {
    "function(place-safepoints),rewrite-statepoints-for-gc,verify<safepoint-ir>"
  } else {
    "function(place-safepoints),rewrite-statepoints-for-gc"
  };

  run_pass_pipeline(module, target_machine, pipeline)?;
  super::debug_lint_module_gc_pointer_discipline(module)?;
  Ok(())
}

/// Backwards-compatible alias for [`place_safepoints_and_rewrite_statepoints_for_gc`].
///
/// This matches the original API name used by the initial `place-safepoints`
/// integration work; keep both so callers can pick whichever naming they prefer.
#[inline]
pub fn place_safepoints_and_rewrite_for_gc(
  module: &Module<'_>,
  target_machine: &TargetMachine,
) -> Result<(), PassError> {
  place_safepoints_and_rewrite_statepoints_for_gc(module, target_machine)
}

fn run_pass_pipeline(
  module: &Module<'_>,
  target_machine: &TargetMachine,
  pipeline: &str,
) -> Result<(), PassError> {
  let pipeline_c = CString::new(pipeline).expect("pipeline contains NUL byte");

  unsafe {
    let options = LLVMCreatePassBuilderOptions();

    let err = LLVMRunPasses(
      module.as_mut_ptr(),
      pipeline_c.as_ptr(),
      target_machine.as_mut_ptr(),
      options,
    );

    LLVMDisposePassBuilderOptions(options);

    if !err.is_null() {
      // LLVMGetErrorMessage consumes the error and returns an owned c-string.
      let msg_ptr = LLVMGetErrorMessage(err);
      let message = CStr::from_ptr(msg_ptr).to_string_lossy().into_owned();
      LLVMDisposeErrorMessage(msg_ptr);

      return Err(PassError::RunPasses {
        pipeline: pipeline.to_owned(),
        message,
      });
    }
  }

  Ok(())
}
