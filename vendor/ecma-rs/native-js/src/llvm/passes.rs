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
  LLVMGetConstOpcode, LLVMGetGC, LLVMGetInstructionOpcode, LLVMGetNextBasicBlock, LLVMGetNextFunction,
  LLVMGetNextInstruction, LLVMGetNumOperands, LLVMGetOperand, LLVMGetStringAttributeAtIndex,
  LLVMGetValueName2, LLVMIsAConstantExpr, LLVMIsAFunction, LLVMIsAInstruction, LLVMIsABitCastInst,
  LLVMPrintValueToString,
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
  #[error(
    "GC-managed function `{function}` contains a non-intrinsic, non-leaf call (expected statepoint or `gc-leaf-function`): {call}\n  extracted_called_operand={called_operand}"
  )]
  StrayCallInGcFunction {
    function: String,
    call: String,
    called_operand: String,
  },
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

  debug_verify_no_stray_calls_in_gc_functions(module)?;
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
  debug_verify_no_stray_calls_in_gc_functions(module)?;
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
      // Peel through instruction pointer casts.
      if !LLVMIsABitCastInst(val).is_null() {
        val = LLVMGetOperand(val, 0);
        continue;
      }
      if !LLVMIsAInstruction(val).is_null()
        && LLVMGetInstructionOpcode(val) == LLVMOpcode::LLVMAddrSpaceCast
      {
        val = LLVMGetOperand(val, 0);
        continue;
      }

      // Peel through constant-expression pointer casts.
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
    // For CallBase-like instructions (call/invoke/callbr), LLVM's operand list places the *called
    // operand* last, with arguments (and operand-bundle inputs / dest blocks) before it.
    let num_ops = LLVMGetNumOperands(inst) as u32;
    debug_assert!(num_ops > 0, "call-like instruction should have at least one operand");
    if num_ops == 0 {
      return inst;
    }
    LLVMGetOperand(inst, num_ops - 1)
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

fn debug_verify_no_stray_calls_in_gc_functions(module: &Module<'_>) -> Result<(), PassError> {
  if !cfg!(debug_assertions) {
    return Ok(());
  }

  unsafe { verify_no_stray_calls_in_gc_functions_raw(module.as_mut_ptr()) }
}

unsafe fn verify_no_stray_calls_in_gc_functions_raw(
  module: llvm_sys::prelude::LLVMModuleRef,
) -> Result<(), PassError> {
  assert!(!module.is_null(), "module must be non-null");

  let mut func = LLVMGetFirstFunction(module);
  while !func.is_null() {
    // Skip declarations.
    if LLVMCountBasicBlocks(func) == 0 {
      func = LLVMGetNextFunction(func);
      continue;
    }

    // Only enforce on GC-managed functions (i.e. those with a `gc "<strategy>"` attribute).
    if LLVMGetGC(func).is_null() {
      func = LLVMGetNextFunction(func);
      continue;
    }

    let func_name = value_name(func);

    let mut bb = LLVMGetFirstBasicBlock(func);
    while !bb.is_null() {
      let mut inst = LLVMGetFirstInstruction(bb);
      while !inst.is_null() {
        let opcode = LLVMGetInstructionOpcode(inst);
        if opcode == LLVMOpcode::LLVMCall || opcode == LLVMOpcode::LLVMInvoke {
          let callee = strip_callee_pointer_casts(get_call_callee_operand(inst));

          // RS4GC should not leave indirect calls behind. The only safe plain calls are to
          // `gc-leaf-function` callees, which requires a direct/global callee.
          if LLVMIsAFunction(callee).is_null() {
            return Err(PassError::StrayCallInGcFunction {
              function: func_name,
              call: value_to_string(inst),
              called_operand: value_to_string(callee),
            });
          }

          if !is_intrinsic_function(callee) && !is_gc_leaf_function(callee) {
            return Err(PassError::StrayCallInGcFunction {
              function: func_name,
              call: value_to_string(inst),
              called_operand: value_to_string(callee),
            });
          }
        }

        inst = LLVMGetNextInstruction(inst);
      }
      bb = LLVMGetNextBasicBlock(bb);
    }

    func = LLVMGetNextFunction(func);
  }

  Ok(())
}

fn is_gc_leaf_function(func: llvm_sys::prelude::LLVMValueRef) -> bool {
  unsafe {
    // Attribute keys in the C API are length-delimited, but must be NUL-terminated.
    const KEY: &[u8] = b"gc-leaf-function\0";
    let attr = LLVMGetStringAttributeAtIndex(
      func,
      llvm_sys::LLVMAttributeFunctionIndex,
      KEY.as_ptr().cast(),
      (KEY.len() - 1) as u32,
    );
    !attr.is_null()
  }
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::llvm::gc;
  use inkwell::context::Context;
  use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
  use inkwell::OptimizationLevel;

  fn host_target_machine() -> TargetMachine {
    crate::llvm::init_native_target().expect("failed to init native LLVM target");

    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).expect("host target");
    let cpu = TargetMachine::get_host_cpu_name().to_string();
    let features = TargetMachine::get_host_cpu_features().to_string();
    target
      .create_target_machine(
        &triple,
        &cpu,
        &features,
        OptimizationLevel::None,
        RelocMode::Default,
        CodeModel::Default,
      )
      .expect("create target machine")
  }

  #[test]
  fn stray_call_verifier_rejects_non_leaf_calls_in_gc_functions() {
    let context = Context::create();
    let module = context.create_module("passes_stray_call_verify");
    let builder = context.create_builder();

    let void_ty = context.void_type();
    let i8_ty = context.i8_type();
    let gc_ptr_ty = gc::gc_ptr_type(&context);

    let callee_ty = void_ty.fn_type(&[], false);
    let callee = module.add_function("callee", callee_ty, None);

    let test_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
    let test_fn = module.add_function("test", test_ty, None);
    gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

    let obj = test_fn
      .get_first_param()
      .expect("missing obj param")
      .into_pointer_value();

    let entry = context.append_basic_block(test_fn, "entry");
    builder.position_at_end(entry);
    builder.build_load(i8_ty, obj, "pre").expect("load");
    builder.build_call(callee, &[], "call").expect("call");
    builder.build_load(i8_ty, obj, "post").expect("load");
    builder.build_return(None).expect("ret void");

    if let Err(err) = module.verify() {
      panic!("input module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
    }

    // Before `rewrite-statepoints-for-gc`, a GC-managed function has a non-leaf call, so the verifier
    // should reject it.
    let err = debug_verify_no_stray_calls_in_gc_functions(&module).unwrap_err();
    assert!(
      matches!(err, PassError::StrayCallInGcFunction { .. }),
      "expected StrayCallInGcFunction, got: {err}"
    );

    let tm = host_target_machine();
    module.set_triple(&tm.get_triple());
    module.set_data_layout(&tm.get_target_data().get_data_layout());

    // After rewriting, the call is wrapped in a statepoint intrinsic, and the verifier should pass.
    rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");
    debug_verify_no_stray_calls_in_gc_functions(&module).expect("verifier should pass after rewrite");
  }
}
