use inkwell::module::Module;
use inkwell::targets::TargetMachine;
use llvm_sys::core::{
  LLVMAddFunction, LLVMAppendBasicBlockInContext, LLVMBuildRetVoid, LLVMCountParamTypes, LLVMCreateBuilderInContext,
  LLVMDisposeBuilder, LLVMFunctionType, LLVMGetFirstBasicBlock, LLVMGetModuleContext, LLVMGetNamedFunction,
  LLVMGetReturnType, LLVMGetTypeKind, LLVMGlobalGetValueType, LLVMIsFunctionVarArg, LLVMPositionBuilderAtEnd,
  LLVMSetLinkage, LLVMVoidTypeInContext,
};
use llvm_sys::error::{LLVMDisposeErrorMessage, LLVMGetErrorMessage};
use llvm_sys::transforms::pass_builder::{
  LLVMCreatePassBuilderOptions, LLVMDisposePassBuilderOptions, LLVMRunPasses,
};
use llvm_sys::LLVMLinkage;
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
///
/// Note: `place-safepoints` only inserts polls when `gc.safepoint_poll` is an
/// *external declaration*. If the module defines a body for the symbol, LLVM may
/// treat it as a GC-leaf function and skip inserting entry/backedge polls
/// entirely. Keep this as a declaration during poll insertion; we can optionally
/// provide a weak stub body after the pass pipeline (for tests that link without
/// `runtime-native`).
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
    } else {
      let ctx = LLVMGetModuleContext(module.as_mut_ptr());
      let void_ty = LLVMVoidTypeInContext(ctx);
      let fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
      LLVMAddFunction(module.as_mut_ptr(), name.as_ptr(), fn_ty);
    }
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
pub fn rewrite_statepoints_for_gc(
  module: &Module<'_>,
  target_machine: &TargetMachine,
) -> Result<(), PassError> {
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
  debug_define_weak_safepoint_poll_stub(module);
  super::debug_lint_module_gc_pointer_discipline(module)?;
  Ok(())
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

fn debug_define_weak_safepoint_poll_stub(module: &Module<'_>) {
  // Regression/linking tests often link generated objects without `runtime-native`. When safepoint
  // polling is enabled, those objects reference `gc.safepoint_poll` (inserted by LLVM
  // `place-safepoints`), so provide a tiny weak definition in debug builds.
  //
  // IMPORTANT: do *not* define the function body before running `place-safepoints`. LLVM will treat
  // a defined `gc.safepoint_poll` as a GC-leaf function and skip inserting polls entirely.
  if !cfg!(debug_assertions) {
    return;
  }

  let name = CString::new("gc.safepoint_poll").expect("gc.safepoint_poll contains NUL");

  unsafe {
    let poll = LLVMGetNamedFunction(module.as_mut_ptr(), name.as_ptr());
    if poll.is_null() {
      return;
    }

    // Only define the stub if the module still has just a declaration.
    if !LLVMGetFirstBasicBlock(poll).is_null() {
      return;
    }

    let ctx = LLVMGetModuleContext(module.as_mut_ptr());
    LLVMSetLinkage(poll, LLVMLinkage::LLVMWeakAnyLinkage);

    let entry_name = CString::new("entry").expect("entry contains NUL");
    let entry = LLVMAppendBasicBlockInContext(ctx, poll, entry_name.as_ptr());
    let builder = LLVMCreateBuilderInContext(ctx);
    LLVMPositionBuilderAtEnd(builder, entry);
    LLVMBuildRetVoid(builder);
    LLVMDisposeBuilder(builder);
  }
}
