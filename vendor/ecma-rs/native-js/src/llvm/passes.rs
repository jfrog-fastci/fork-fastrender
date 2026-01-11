use inkwell::module::Module;
use inkwell::targets::TargetMachine;
use llvm_sys::core::{
  LLVMAddFunction, LLVMAppendBasicBlockInContext, LLVMBuildRetVoid, LLVMCountParamTypes, LLVMCreateBuilderInContext,
  LLVMDisposeBuilder, LLVMFunctionType, LLVMGetFirstBasicBlock, LLVMGetModuleContext, LLVMGetNamedFunction,
  LLVMGetReturnType, LLVMGetTypeKind, LLVMGlobalGetValueType, LLVMIsFunctionVarArg, LLVMPositionBuilderAtEnd,
  LLVMSetLinkage, LLVMVoidTypeInContext,
};
use llvm_sys::core::{
  LLVMCountBasicBlocks, LLVMDisposeMessage, LLVMGetFirstFunction, LLVMGetFirstInstruction,
  LLVMGetConstOpcode, LLVMGetInstructionOpcode, LLVMGetNextBasicBlock, LLVMGetNextFunction,
  LLVMGetNextInstruction, LLVMGetNumArgOperands, LLVMGetOperand, LLVMGetValueName2,
  LLVMIsAConstantExpr, LLVMIsAFunction, LLVMPrintValueToString,
};
use llvm_sys::error::{LLVMDisposeErrorMessage, LLVMGetErrorMessage};
use llvm_sys::transforms::pass_builder::{
  LLVMCreatePassBuilderOptions, LLVMDisposePassBuilderOptions, LLVMRunPasses,
};
use llvm_sys::{LLVMOpcode, LLVMLinkage};
use std::ffi::{CStr, CString};
use std::ptr;

#[derive(Debug, thiserror::Error)]
pub enum PassError {
  #[error(transparent)]
  GcLint(#[from] super::LintError),
  #[error(transparent)]
  GcCallsiteInvariant(#[from] CallsiteInvariantError),
  #[error("LLVMRunPasses failed for pipeline `{pipeline}`: {message}")]
  RunPasses { pipeline: String, message: String },
  #[error("LLVM module verification failed after pipeline `{pipeline}`: {message}")]
  Verify { pipeline: String, message: String },
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
  if let Err(message) = module.verify() {
    return Err(PassError::Verify {
      pipeline: pipeline.to_owned(),
      message: message.to_string(),
    });
  }

  super::debug_lint_module_gc_pointer_discipline(module)?;
  verify_no_stray_calls_in_ts_generated_functions(module)?;
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
  if let Err(message) = module.verify() {
    return Err(PassError::Verify {
      pipeline: pipeline.to_owned(),
      message: message.to_string(),
    });
  }
  debug_define_weak_safepoint_poll_stub(module);
  super::debug_lint_module_gc_pointer_discipline(module)?;
  verify_no_stray_calls_in_ts_generated_functions(module)?;
  Ok(())
}

// -----------------------------------------------------------------------------
// Post-statepoint rewrite verifier: TS callsites must not be plain calls.
// -----------------------------------------------------------------------------

/// Error raised when a TS-generated function contains a stray `call`/`invoke` that was not rewritten
/// into a `gc.statepoint`.
#[derive(Debug, thiserror::Error)]
pub enum CallsiteInvariantError {
  #[error("TS-generated function `{function}` contains a non-statepoint call/invoke after rewrite: {instruction}")]
  StrayCall { function: String, instruction: String },
}

/// Return true when a function name is considered "TS-generated" for the purpose of GC safepoint
/// invariants.
///
/// We currently key off `native-js`'s stable symbol naming scheme:
/// - `__nativejs_def_<defid-hex>...` for TS definitions
/// - `__nativejs_file_init_<fileid-hex>` for module initializers
fn is_ts_generated_function_name(name: &str) -> bool {
  name.starts_with("__nativejs_def_") || name.starts_with("__nativejs_file_init_")
}

fn value_name(val: llvm_sys::prelude::LLVMValueRef) -> String {
  unsafe {
    let mut len: usize = 0;
    let ptr = LLVMGetValueName2(val, &mut len as *mut usize);
    if ptr.is_null() || len == 0 {
      return "<anon>".to_string();
    }
    let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
    String::from_utf8_lossy(bytes).to_string()
  }
}

fn value_to_string(val: llvm_sys::prelude::LLVMValueRef) -> String {
  unsafe {
    let s = LLVMPrintValueToString(val);
    if s.is_null() {
      return "<unprintable>".to_string();
    }
    let out = CStr::from_ptr(s).to_string_lossy().into_owned();
    LLVMDisposeMessage(s);
    out
  }
}

fn strip_callee_pointer_casts(
  mut val: llvm_sys::prelude::LLVMValueRef,
) -> llvm_sys::prelude::LLVMValueRef {
  unsafe {
    loop {
      if !LLVMIsAConstantExpr(val).is_null() {
        match LLVMGetConstOpcode(val) {
          LLVMOpcode::LLVMBitCast | LLVMOpcode::LLVMAddrSpaceCast => {
            val = LLVMGetOperand(val, 0);
            continue;
          }
          _ => {}
        }
      }
      return val;
    }
  }
}

fn get_call_callee_operand(inst: llvm_sys::prelude::LLVMValueRef) -> llvm_sys::prelude::LLVMValueRef {
  unsafe {
    // LLVM's C API treats "called value" differently across instruction kinds. For `call`/`invoke`
    // it is *usually* operand 0, but for some intrinsics (notably statepoints) the called value can
    // appear after the argument operands.
    //
    // Use a simple heuristic:
    // - prefer whichever candidate strips to a `Function`
    // - otherwise default to operand 0 (covers indirect calls).
    let op0 = LLVMGetOperand(inst, 0);
    let op_n = LLVMGetOperand(inst, LLVMGetNumArgOperands(inst));

    let op0_is_fn = !LLVMIsAFunction(strip_callee_pointer_casts(op0)).is_null();
    let op_n_is_fn = !LLVMIsAFunction(strip_callee_pointer_casts(op_n)).is_null();

    match (op0_is_fn, op_n_is_fn) {
      (true, false) => op0,
      (false, true) => op_n,
      _ => op0,
    }
  }
}

fn is_intrinsic_function(val: llvm_sys::prelude::LLVMValueRef) -> bool {
  unsafe {
    // Calls emitted by LLVM's statepoint rewrite can reference intrinsics through constant-expression
    // casts (e.g. `bitcast`). Strip those before checking the callee name.
    let val = strip_callee_pointer_casts(val);

    // Intrinsics are named `llvm.*` (including `llvm.experimental.*`).
    //
    // Note: some callees are represented as constant expressions or aliases and don't show up as a
    // `Function` in the C API. Prefer the symbol name when available and fall back to a printed
    // value check (e.g. `ptr @llvm.foo`).
    let name = value_name(val);
    if name.starts_with("llvm.") {
      return true;
    }
    if !LLVMIsAFunction(val).is_null() {
      return false;
    }
    value_to_string(val).contains("@llvm.")
  }
}

/// Enforce the "all TS calls are statepoints" invariant.
///
/// Why this exists:
/// - During GC, only the *top* frame is guaranteed to be stopped at a safepoint instruction.
/// - Older frames are suspended at their *callsite return addresses*.
/// - If a TS-generated function contains a plain call (not a statepoint), that return address will
///   not correspond to a stackmap record, making precise stack scanning unsound.
///
/// This verifier runs **after** `rewrite-statepoints-for-gc` (see above pass pipelines) and rejects
/// any remaining non-intrinsic `call`/`invoke` in TS-generated functions.
fn verify_no_stray_calls_in_ts_generated_functions(
  module: &Module<'_>,
) -> Result<(), CallsiteInvariantError> {
  unsafe {
    let mut func = LLVMGetFirstFunction(module.as_mut_ptr());
    while !func.is_null() {
      // Skip declarations.
      if LLVMCountBasicBlocks(func) == 0 {
        func = LLVMGetNextFunction(func);
        continue;
      }

      let func_name = value_name(func);
      if !is_ts_generated_function_name(&func_name) {
        func = LLVMGetNextFunction(func);
        continue;
      }

      let mut bb = LLVMGetFirstBasicBlock(func);
      while !bb.is_null() {
        let mut inst = LLVMGetFirstInstruction(bb);
      while !inst.is_null() {
          match LLVMGetInstructionOpcode(inst) {
            LLVMOpcode::LLVMCall | LLVMOpcode::LLVMInvoke | LLVMOpcode::LLVMCallBr => {
              let callee = get_call_callee_operand(inst);
              if !is_intrinsic_function(callee) {
                return Err(CallsiteInvariantError::StrayCall {
                  function: func_name,
                  instruction: value_to_string(inst),
                });
              }
            }
            _ => {}
          }

          inst = LLVMGetNextInstruction(inst);
        }
        bb = LLVMGetNextBasicBlock(bb);
      }

      func = LLVMGetNextFunction(func);
    }
  }

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
