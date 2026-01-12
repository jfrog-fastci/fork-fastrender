use inkwell::module::Module;
use inkwell::targets::TargetMachine;
use llvm_sys::core::{
  LLVMAddFunction, LLVMAddGlobal, LLVMAddIncoming, LLVMAppendBasicBlockInContext, LLVMBuildAnd, LLVMBuildBr,
  LLVMBuildCall2, LLVMBuildCondBr, LLVMBuildICmp, LLVMBuildLoad2, LLVMBuildPhi, LLVMBuildRetVoid, LLVMConstInt,
  LLVMCountBasicBlocks, LLVMCountIncoming, LLVMCountParamTypes, LLVMCreateBuilderInContext, LLVMDeleteBasicBlock,
  LLVMDisposeBuilder,
  LLVMDisposeMessage, LLVMFunctionType, LLVMGetBasicBlockParent, LLVMGetBasicBlockTerminator, LLVMGetConstOpcode,
  LLVMGetFirstBasicBlock, LLVMGetFirstFunction, LLVMGetFirstInstruction, LLVMGetGC, LLVMGetIncomingBlock,
  LLVMGetIncomingValue, LLVMGetInitializer, LLVMGetInstructionOpcode, LLVMGetInstructionParent, LLVMGetIntTypeWidth,
  LLVMGetModuleContext, LLVMGetNamedFunction, LLVMGetNamedGlobal, LLVMGetNextBasicBlock, LLVMGetNextFunction,
  LLVMGetNextInstruction, LLVMGetTailCallKind,
  LLVMGetNumOperands, LLVMGetNumSuccessors, LLVMGetOperand, LLVMGetParamTypes, LLVMGetReturnType,
  LLVMGetStringAttributeAtIndex, LLVMGetValueKind,
  LLVMGetSuccessor, LLVMGetTypeKind, LLVMGetValueName2, LLVMGlobalGetValueType, LLVMInsertIntoBuilder,
  LLVMInstructionEraseFromParent, LLVMInstructionRemoveFromParent, LLVMInt64TypeInContext, LLVMIsAConstantExpr,
  LLVMIsAFunction, LLVMIsAInstruction, LLVMIsABitCastInst, LLVMIsFunctionVarArg, LLVMPositionBuilderAtEnd,
  LLVMPositionBuilderBefore, LLVMPrintValueToString, LLVMReplaceAllUsesWith, LLVMSetAlignment, LLVMSetInitializer,
  LLVMSetLinkage, LLVMSetOrdering, LLVMSetTailCallKind, LLVMTypeOf, LLVMVoidTypeInContext,
};
use llvm_sys::error::{LLVMDisposeErrorMessage, LLVMGetErrorMessage};
use llvm_sys::prelude::{LLVMBasicBlockRef, LLVMValueRef};
use llvm_sys::transforms::pass_builder::{
  LLVMCreatePassBuilderOptions, LLVMDisposePassBuilderOptions, LLVMRunPasses,
};
use llvm_sys::{
  LLVMAtomicOrdering, LLVMIntPredicate, LLVMLinkage, LLVMTailCallKind, LLVMOpcode, LLVMTypeKind,
  LLVMValueKind,
};
use std::ffi::{CStr, CString};
use std::ptr;

// -----------------------------------------------------------------------------
// Tiny helpers
// -----------------------------------------------------------------------------

/// Internal helper for passing static C string literals to llvm-sys APIs.
///
/// `LLVMBuild*` functions require NUL-terminated C strings. For fixed string literals we can avoid
/// an allocation by appending `\0` at compile time.
macro_rules! c_str {
  ($lit:literal) => {
    concat!($lit, "\0").as_ptr() as *const ::std::os::raw::c_char
  };
}

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
  #[error(
    "GC-managed function `{function}` contains a `callbr` instruction, which LLVM 18's `rewrite-statepoints-for-gc` pass can crash on: {instruction}"
  )]
  UnsupportedCallBrInGcFunction { function: String, instruction: String },
  #[error(
    "GC-managed function `{function}` contains a call to inline asm, which LLVM 18's `rewrite-statepoints-for-gc` pass cannot rewrite without aborting: {instruction}"
  )]
  UnsupportedInlineAsmInGcFunction { function: String, instruction: String },
  #[error(
    "GC-managed function `{function}` contains a `musttail` call, which LLVM 18's `rewrite-statepoints-for-gc` pass cannot rewrite without aborting: {instruction}"
  )]
  UnsupportedMustTailCallInGcFunction { function: String, instruction: String },
  #[error("LLVM module defines `{name}` with incompatible signature (expected `void ()`)")]
  IncompatibleSafepointPollSignature { name: String },
  #[error("LLVM module defines `{name}` with incompatible type (expected `i64`)")]
  IncompatibleSafepointEpochType { name: String },
  #[error("LLVM module defines `{name}` with incompatible signature (expected `void (i64)`)")]
  IncompatibleSafepointSlowSignature { name: String },
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

      // `place-safepoints` only inserts polls when `gc.safepoint_poll` is a declaration (has no
      // body). In debug builds/tests we may have defined a weak stub body after a previous run, so
      // strip any existing blocks here to ensure poll insertion remains effective.
      let mut bb = LLVMGetFirstBasicBlock(existing);
      while !bb.is_null() {
        let next = LLVMGetNextBasicBlock(bb);
        LLVMDeleteBasicBlock(bb);
        bb = next;
      }

      // If a previous run defined a weak stub body, it likely used a definition-only linkage
      // (e.g. `weak`), which becomes invalid once we strip the body. Normalize to a plain external
      // declaration.
      LLVMSetLinkage(existing, LLVMLinkage::LLVMExternalLinkage);
    } else {
      let ctx = LLVMGetModuleContext(module.as_mut_ptr());
      let void_ty = LLVMVoidTypeInContext(ctx);
      let fn_ty = LLVMFunctionType(void_ty, ptr::null_mut(), 0, 0);
      LLVMAddFunction(module.as_mut_ptr(), name.as_ptr(), fn_ty);
    }
  }

  Ok(())
}

fn reject_callbr_in_gc_functions(module: &Module<'_>) -> Result<(), PassError> {
  unsafe {
    let mut func = LLVMGetFirstFunction(module.as_mut_ptr());
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
          if opcode == LLVMOpcode::LLVMCallBr {
            return Err(PassError::UnsupportedCallBrInGcFunction {
              function: func_name,
              instruction: value_to_string(inst),
            });
          }
          if opcode == LLVMOpcode::LLVMCall
            && LLVMGetTailCallKind(inst) == LLVMTailCallKind::LLVMTailCallKindMustTail
          {
            return Err(PassError::UnsupportedMustTailCallInGcFunction {
              function: func_name,
              instruction: value_to_string(inst),
            });
          }
          if opcode == LLVMOpcode::LLVMCall || opcode == LLVMOpcode::LLVMInvoke {
            let callee = strip_callee_pointer_casts(get_call_callee_operand(inst));
            if LLVMGetValueKind(callee) == LLVMValueKind::LLVMInlineAsmValueKind {
              return Err(PassError::UnsupportedInlineAsmInGcFunction {
                function: func_name,
                instruction: value_to_string(inst),
              });
            }
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
  reject_callbr_in_gc_functions(module)?;

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

/// Runs `place-safepoints` + poll lowering + `rewrite-statepoints-for-gc` on `module`.
///
/// This is the "easy correctness" path for making GC-managed functions safe:
/// - `place-safepoints` inserts poll call *markers* (`@gc.safepoint_poll`) at
///   entry and loop backedges (including counted loops; `native-js` enables
///   `--spp-all-backedges`).
/// - `native-js` lowers each marker into an inline `@RT_GC_EPOCH` load+branch
///   with a slow-path call to `@rt_gc_safepoint_slow(i64 epoch)`.
/// - `rewrite-statepoints-for-gc` converts the slow-path call (and any other
///   calls) into `llvm.experimental.gc.statepoint.*` so stack maps/relocations
///   are emitted.
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
  reject_callbr_in_gc_functions(module)?;

  ensure_gc_safepoint_poll_decl(module)?;

  // Run `place-safepoints` first so LLVM inserts `call void @gc.safepoint_poll()` markers at entry
  // and loop backedges. We then rewrite those markers into an inline epoch check
  // (`@RT_GC_EPOCH`) and a slow-path call (`@rt_gc_safepoint_slow(i64)`), so the fast path has no
  // call/statepoint overhead. Finally, `rewrite-statepoints-for-gc` rewrites the slow-path call
  // into a statepoint so stackmaps/relocations are emitted.
  run_pass_pipeline(module, target_machine, "function(place-safepoints)")?;
  rewrite_safepoint_polls_to_inline_epoch_checks(module)?;
  if cfg!(debug_assertions) {
    if let Err(message) = module.verify() {
      return Err(PassError::Verify {
        pipeline: "poll lowering".to_string(),
        message: message.to_string(),
      });
    }
  }

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
  debug_define_weak_safepoint_poll_stub(module);
  debug_define_weak_rt_gc_epoch_stub(module);
  debug_define_weak_rt_gc_safepoint_slow_stub(module);
  debug_verify_no_stray_calls_in_gc_functions(module)?;
  super::debug_lint_module_gc_pointer_discipline(module)?;
  verify_no_stray_calls_in_ts_generated_functions(module)?;
  Ok(())
}

// -----------------------------------------------------------------------------
// Safepoint poll lowering: `gc.safepoint_poll` -> inline epoch check + slow path.
// -----------------------------------------------------------------------------

fn ensure_rt_gc_epoch_decl(module: &Module<'_>) -> Result<LLVMValueRef, PassError> {
  let name = CString::new("RT_GC_EPOCH").expect("RT_GC_EPOCH contains NUL");
  unsafe {
    let existing = LLVMGetNamedGlobal(module.as_mut_ptr(), name.as_ptr());
    if !existing.is_null() {
      let ty = LLVMGlobalGetValueType(existing);
      if LLVMGetTypeKind(ty) != LLVMTypeKind::LLVMIntegerTypeKind || LLVMGetIntTypeWidth(ty) != 64 {
        return Err(PassError::IncompatibleSafepointEpochType {
          name: "RT_GC_EPOCH".to_string(),
        });
      }
      return Ok(existing);
    }

    let ctx = LLVMGetModuleContext(module.as_mut_ptr());
    let i64_ty = LLVMInt64TypeInContext(ctx);
    Ok(LLVMAddGlobal(module.as_mut_ptr(), i64_ty, name.as_ptr()))
  }
}

fn ensure_rt_gc_safepoint_slow_decl(module: &Module<'_>) -> Result<LLVMValueRef, PassError> {
  let name = CString::new("rt_gc_safepoint_slow").expect("rt_gc_safepoint_slow contains NUL");

  unsafe {
    let existing = LLVMGetNamedFunction(module.as_mut_ptr(), name.as_ptr());
    if !existing.is_null() {
      // Ensure the signature matches `declare void @rt_gc_safepoint_slow(i64)`.
      let fn_ty = LLVMGlobalGetValueType(existing);
      let ret_ty = LLVMGetReturnType(fn_ty);
      let param_count = LLVMCountParamTypes(fn_ty);
      let varargs = LLVMIsFunctionVarArg(fn_ty) != 0;
      if LLVMGetTypeKind(ret_ty) != LLVMTypeKind::LLVMVoidTypeKind || param_count != 1 || varargs {
        return Err(PassError::IncompatibleSafepointSlowSignature {
          name: "rt_gc_safepoint_slow".to_string(),
        });
      }

      let mut params = [ptr::null_mut()];
      LLVMGetParamTypes(fn_ty, params.as_mut_ptr());
      let param_ty = params[0];
      if LLVMGetTypeKind(param_ty) != LLVMTypeKind::LLVMIntegerTypeKind
        || LLVMGetIntTypeWidth(param_ty) != 64
      {
        return Err(PassError::IncompatibleSafepointSlowSignature {
          name: "rt_gc_safepoint_slow".to_string(),
        });
      }
      return Ok(existing);
    }

    let ctx = LLVMGetModuleContext(module.as_mut_ptr());
    let void_ty = LLVMVoidTypeInContext(ctx);
    let i64_ty = LLVMInt64TypeInContext(ctx);
    let mut params = [i64_ty];
    let fn_ty = LLVMFunctionType(void_ty, params.as_mut_ptr(), 1, 0);
    Ok(LLVMAddFunction(module.as_mut_ptr(), name.as_ptr(), fn_ty))
  }
}

fn rewrite_safepoint_polls_to_inline_epoch_checks(module: &Module<'_>) -> Result<(), PassError> {
  // `place-safepoints` inserts unconditional `call void @gc.safepoint_poll()` at entry/backedges.
  // Calling a statepoint at every loop iteration is too expensive; instead, lower each poll into:
  //
  //   epoch = load atomic @RT_GC_EPOCH (Acquire)
  //   if (epoch & 1) rt_gc_safepoint_slow(epoch)
  //
  // The slow-path call is then rewritten into a statepoint by `rewrite-statepoints-for-gc`, so
  // only the slow path carries statepoint/stackmap overhead.
  let poll_name = CString::new("gc.safepoint_poll").expect("gc.safepoint_poll contains NUL");

  unsafe {
    let poll_fn = LLVMGetNamedFunction(module.as_mut_ptr(), poll_name.as_ptr());
    if poll_fn.is_null() {
      return Ok(());
    }

    let rt_gc_epoch = ensure_rt_gc_epoch_decl(module)?;
    let rt_gc_safepoint_slow = ensure_rt_gc_safepoint_slow_decl(module)?;

    let ctx = LLVMGetModuleContext(module.as_mut_ptr());
    let i64_ty = LLVMInt64TypeInContext(ctx);

    // Collect poll call instructions first; rewriting mutates the CFG.
    let mut poll_calls: Vec<LLVMValueRef> = Vec::new();
    let mut func = LLVMGetFirstFunction(module.as_mut_ptr());
    while !func.is_null() {
      // Skip declarations.
      if LLVMCountBasicBlocks(func) == 0 {
        func = LLVMGetNextFunction(func);
        continue;
      }

      let mut bb = LLVMGetFirstBasicBlock(func);
      while !bb.is_null() {
        let mut inst = LLVMGetFirstInstruction(bb);
        while !inst.is_null() {
          if LLVMGetInstructionOpcode(inst) == LLVMOpcode::LLVMCall {
            // Poll calls have no arguments, so the callee is operand 0.
            let callee = LLVMGetOperand(inst, 0);
            if callee == poll_fn {
              poll_calls.push(inst);
            }
          }
          inst = LLVMGetNextInstruction(inst);
        }
        bb = LLVMGetNextBasicBlock(bb);
      }
      func = LLVMGetNextFunction(func);
    }

    for poll_call in poll_calls {
      rewrite_one_safepoint_poll_call(ctx, i64_ty, rt_gc_epoch, rt_gc_safepoint_slow, poll_call);
    }
  }

  Ok(())
}

unsafe fn rewrite_one_safepoint_poll_call(
  ctx: llvm_sys::prelude::LLVMContextRef,
  i64_ty: llvm_sys::prelude::LLVMTypeRef,
  rt_gc_epoch: LLVMValueRef,
  rt_gc_safepoint_slow: LLVMValueRef,
  poll_call: LLVMValueRef,
) {
  let check_bb = LLVMGetInstructionParent(poll_call);
  if check_bb.is_null() {
    return;
  }

  // Split `check_bb` at the poll call so the poll becomes the first instruction of the slow block.
  let slow_name = CString::new("gc.poll.slow").expect("gc.poll.slow contains NUL");
  let slow_bb = split_basic_block(ctx, check_bb, poll_call, slow_name.as_ptr());

  // Split the slow block at the instruction after the poll call to form a continuation block.
  let after_poll = LLVMGetNextInstruction(poll_call);
  if after_poll.is_null() {
    return;
  }
  let cont_name = CString::new("gc.poll.cont").expect("gc.poll.cont contains NUL");
  let cont_bb = split_basic_block(ctx, slow_bb, after_poll, cont_name.as_ptr());

  // Replace the unconditional branch inserted by the first split with our inline epoch check.
  let term = LLVMGetBasicBlockTerminator(check_bb);
  if !term.is_null() {
    LLVMInstructionEraseFromParent(term);
  }

  let builder = LLVMCreateBuilderInContext(ctx);

  // Inline poll in `check_bb`.
  LLVMPositionBuilderAtEnd(builder, check_bb);
  let epoch = LLVMBuildLoad2(builder, i64_ty, rt_gc_epoch, c_str!("gc.epoch"));
  LLVMSetOrdering(epoch, LLVMAtomicOrdering::LLVMAtomicOrderingAcquire);
  // Ensure the atomic load is sufficiently aligned even if the module is missing a datalayout.
  LLVMSetAlignment(epoch, 8);

  let one = LLVMConstInt(i64_ty, 1, 0);
  let lowbit = LLVMBuildAnd(builder, epoch, one, c_str!("gc.epoch.lowbit"));
  let zero = LLVMConstInt(i64_ty, 0, 0);
  let requested = LLVMBuildICmp(
    builder,
    LLVMIntPredicate::LLVMIntNE,
    lowbit,
    zero,
    c_str!("gc.poll.requested"),
  );
  LLVMBuildCondBr(builder, requested, slow_bb, cont_bb);

  // Replace the marker call in the slow block with `rt_gc_safepoint_slow(epoch)`.
  LLVMInstructionEraseFromParent(poll_call);
  // Insert before the block's existing terminator (branch to `cont_bb`).
  let slow_term = LLVMGetBasicBlockTerminator(slow_bb);
  if !slow_term.is_null() {
    LLVMPositionBuilderBefore(builder, slow_term);
  } else {
    LLVMPositionBuilderAtEnd(builder, slow_bb);
  }
  let slow_fn_ty = LLVMGlobalGetValueType(rt_gc_safepoint_slow);
  let mut args = [epoch];
  let call = LLVMBuildCall2(
    builder,
    slow_fn_ty,
    rt_gc_safepoint_slow,
    args.as_mut_ptr(),
    1,
    // Void calls cannot be assigned an SSA name (verifier rejects `%x = call void ...`).
    c_str!(""),
  );
  LLVMSetTailCallKind(call, LLVMTailCallKind::LLVMTailCallKindNoTail);

  // `slow_bb` already ends with an unconditional branch to `cont_bb` inserted by the second split.
  LLVMDisposeBuilder(builder);
}

unsafe fn split_basic_block(
  ctx: llvm_sys::prelude::LLVMContextRef,
  bb: LLVMBasicBlockRef,
  split_at: LLVMValueRef,
  name: *const ::std::os::raw::c_char,
) -> LLVMBasicBlockRef {
  // `llvm-sys` does not currently expose `LLVMSplitBasicBlock`, and it is not guaranteed to be
  // available in the linked LLVM libraries. Implement the required behavior using the stable C API:
  //
  // - Create a new block in the same function.
  // - Move `split_at` and all following instructions into it.
  // - Add an unconditional branch from `bb` to the new block.
  // - Update PHI nodes in the new block's successors to replace `bb` with the new block as the
  //   incoming predecessor.

  let func = LLVMGetBasicBlockParent(bb);
  assert!(!func.is_null(), "basic block must have a parent function");
  let new_bb = LLVMAppendBasicBlockInContext(ctx, func, name);

  // Move instructions starting at `split_at` into `new_bb`, preserving order.
  let builder = LLVMCreateBuilderInContext(ctx);
  LLVMPositionBuilderAtEnd(builder, new_bb);

  let mut inst = split_at;
  while !inst.is_null() {
    let next = LLVMGetNextInstruction(inst);
    LLVMInstructionRemoveFromParent(inst);
    LLVMInsertIntoBuilder(builder, inst);
    inst = next;
  }
  LLVMDisposeBuilder(builder);

  // `bb` no longer has a terminator; branch to the new block.
  let builder = LLVMCreateBuilderInContext(ctx);
  LLVMPositionBuilderAtEnd(builder, bb);
  LLVMBuildBr(builder, new_bb);
  LLVMDisposeBuilder(builder);

  // Fix up successor PHI nodes: any PHI input that previously came from `bb` must now come from
  // `new_bb` (since `bb` is no longer a predecessor of those blocks).
  let term = LLVMGetBasicBlockTerminator(new_bb);
  if !term.is_null() {
    let succ_count = LLVMGetNumSuccessors(term);
    for i in 0..succ_count {
      let succ = LLVMGetSuccessor(term, i);
      if succ.is_null() {
        continue;
      }
      replace_phi_incoming_block(ctx, succ, bb, new_bb);
    }
  }

  new_bb
}

unsafe fn replace_phi_incoming_block(
  ctx: llvm_sys::prelude::LLVMContextRef,
  bb: LLVMBasicBlockRef,
  old_pred: LLVMBasicBlockRef,
  new_pred: LLVMBasicBlockRef,
) {
  // Collect leading PHI nodes (they are guaranteed to appear at the start of the block). Keep track
  // of the first non-PHI instruction so we can insert the replacement PHIs *after* the existing PHI
  // range. This avoids using an insertion point that gets deleted while we erase old PHIs.
  let mut phis: Vec<LLVMValueRef> = Vec::new();
  let mut insert_before = LLVMGetFirstInstruction(bb);
  while !insert_before.is_null() && LLVMGetInstructionOpcode(insert_before) == LLVMOpcode::LLVMPHI {
    phis.push(insert_before);
    insert_before = LLVMGetNextInstruction(insert_before);
  }

  if phis.is_empty() {
    return;
  }

  // Create replacement PHIs first, then replace uses + erase the old PHIs. Erasing while building
  // can invalidate the IRBuilder insertion point and crash inside LLVM.
  let builder = LLVMCreateBuilderInContext(ctx);
  if !insert_before.is_null() {
    LLVMPositionBuilderBefore(builder, insert_before);
  } else {
    LLVMPositionBuilderAtEnd(builder, bb);
  }

  let mut replacements: Vec<(LLVMValueRef, LLVMValueRef)> = Vec::with_capacity(phis.len());

  for &phi in &phis {
    let ty = LLVMTypeOf(phi);
    let incoming = LLVMCountIncoming(phi);
    let mut values: Vec<LLVMValueRef> = Vec::with_capacity(incoming as usize);
    let mut blocks: Vec<LLVMBasicBlockRef> = Vec::with_capacity(incoming as usize);
    for i in 0..incoming {
      let mut blk = LLVMGetIncomingBlock(phi, i);
      if blk == old_pred {
        blk = new_pred;
      }
      values.push(LLVMGetIncomingValue(phi, i));
      blocks.push(blk);
    }

    let new_phi = LLVMBuildPhi(builder, ty, c_str!(""));
    LLVMAddIncoming(
      new_phi,
      values.as_mut_ptr(),
      blocks.as_mut_ptr(),
      incoming,
    );
    replacements.push((phi, new_phi));
  }

  LLVMDisposeBuilder(builder);

  for (old_phi, new_phi) in &replacements {
    LLVMReplaceAllUsesWith(*old_phi, *new_phi);
  }
  for (old_phi, _new_phi) in replacements {
    LLVMInstructionEraseFromParent(old_phi);
  }
}

// -----------------------------------------------------------------------------
// Post-statepoint rewrite verifier: TS callsites must not be plain calls.
// -----------------------------------------------------------------------------

/// Error raised when a TS-generated function contains a stray `call`/`invoke` that was not rewritten
/// into a `gc.statepoint` (and is not explicitly marked as a `"gc-leaf-function"` callsite).
#[derive(Debug, thiserror::Error)]
pub enum CallsiteInvariantError {
  #[error(
    "TS-generated function `{function}` contains a call/invoke after rewrite that is neither a statepoint nor a `gc-leaf-function`: {instruction}"
  )]
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

fn is_gc_leaf_function(val: llvm_sys::prelude::LLVMValueRef) -> bool {
  unsafe {
    if LLVMIsAFunction(val).is_null() {
      return false;
    }

    // Attribute keys in the C API are length-delimited, but must be NUL-terminated.
    const KEY: &[u8] = b"gc-leaf-function\0";
    let attr = LLVMGetStringAttributeAtIndex(
      val,
      llvm_sys::LLVMAttributeFunctionIndex,
      KEY.as_ptr().cast(),
      (KEY.len() - 1) as u32,
    );
    !attr.is_null()
  }
}

/// Enforce the "TS-generated callsites are GC-correct" invariant.
///
/// Why this exists:
/// - During GC, only the *top* frame is guaranteed to be stopped at a safepoint instruction.
/// - Older frames are suspended at their *callsite return addresses*.
/// - If a TS-generated function contains a plain call (not a statepoint) to a callee that may
///   trigger GC, that return address will not correspond to a stackmap record, making precise stack
///   scanning unsound.
///
/// Exception: calls to callees annotated `"gc-leaf-function"` are allowed to remain plain calls
/// because such callees must not allocate/safepoint/GC, so the caller's return address cannot be a
/// GC trigger point.
///
/// This verifier runs **after** `rewrite-statepoints-for-gc` (see above pass pipelines) and rejects
/// any remaining non-intrinsic, non-leaf `call`/`invoke` in TS-generated functions.
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
              let callee = strip_callee_pointer_casts(callee);
              let is_leaf = !LLVMIsAFunction(callee).is_null() && is_gc_leaf_function(callee);
              if !is_intrinsic_function(callee) && !is_leaf {
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

/// Run the post-RS4GC "no stray calls in GC-managed functions" verifier in debug builds/tests.
///
/// In release builds this is a no-op unless the `gc-callsite-verify` feature is enabled.
#[cfg(any(debug_assertions, feature = "gc-callsite-verify"))]
fn debug_verify_no_stray_calls_in_gc_functions(module: &Module<'_>) -> Result<(), PassError> {
  unsafe { verify_no_stray_calls_in_gc_functions_raw(module.as_mut_ptr()) }
}

#[cfg(not(any(debug_assertions, feature = "gc-callsite-verify")))]
fn debug_verify_no_stray_calls_in_gc_functions(_module: &Module<'_>) -> Result<(), PassError> {
  Ok(())
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
        if opcode == LLVMOpcode::LLVMCall
          || opcode == LLVMOpcode::LLVMInvoke
          || opcode == LLVMOpcode::LLVMCallBr
        {
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

fn debug_define_weak_rt_gc_epoch_stub(module: &Module<'_>) {
  // Like `debug_define_weak_safepoint_poll_stub`, but for the exported safepoint epoch global used
  // by the fast-path poll lowering.
  if !cfg!(debug_assertions) {
    return;
  }

  let name = CString::new("RT_GC_EPOCH").expect("RT_GC_EPOCH contains NUL");

  unsafe {
    let epoch = LLVMGetNamedGlobal(module.as_mut_ptr(), name.as_ptr());
    if epoch.is_null() {
      return;
    }

    // Only define a stub if this is still just a declaration.
    if !LLVMGetInitializer(epoch).is_null() {
      return;
    }

    let ctx = LLVMGetModuleContext(module.as_mut_ptr());
    let i64_ty = LLVMInt64TypeInContext(ctx);
    LLVMSetLinkage(epoch, LLVMLinkage::LLVMWeakAnyLinkage);
    LLVMSetInitializer(epoch, LLVMConstInt(i64_ty, 0, 0));
  }
}

fn debug_define_weak_rt_gc_safepoint_slow_stub(module: &Module<'_>) {
  // Provide a weak no-op definition of `rt_gc_safepoint_slow` for tests that link generated objects
  // without `runtime-native`.
  if !cfg!(debug_assertions) {
    return;
  }

  let name = CString::new("rt_gc_safepoint_slow").expect("rt_gc_safepoint_slow contains NUL");

  unsafe {
    let slow = LLVMGetNamedFunction(module.as_mut_ptr(), name.as_ptr());
    if slow.is_null() {
      return;
    }

    // Only define the stub if the module still has just a declaration.
    if !LLVMGetFirstBasicBlock(slow).is_null() {
      return;
    }

    let ctx = LLVMGetModuleContext(module.as_mut_ptr());
    LLVMSetLinkage(slow, LLVMLinkage::LLVMWeakAnyLinkage);

    let entry_name = CString::new("entry").expect("entry contains NUL");
    let entry = LLVMAppendBasicBlockInContext(ctx, slow, entry_name.as_ptr());
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
  use inkwell::attributes::AttributeLoc;
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

  #[test]
  fn stray_call_verifier_allows_leaf_calls_in_gc_functions() {
    let context = Context::create();
    let module = context.create_module("passes_stray_call_verify_leaf");
    let builder = context.create_builder();

    let void_ty = context.void_type();
    let i8_ty = context.i8_type();
    let gc_ptr_ty = gc::gc_ptr_type(&context);

    let callee_ty = void_ty.fn_type(&[], false);
    let callee = module.add_function("callee", callee_ty, None);
    let leaf = context.create_string_attribute("gc-leaf-function", "");
    callee.add_attribute(AttributeLoc::Function, leaf);

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

    // Leaf calls are allowed to remain as plain calls even in GC-managed functions.
    debug_verify_no_stray_calls_in_gc_functions(&module).expect("verifier should allow leaf calls");

    let tm = host_target_machine();
    module.set_triple(&tm.get_triple());
    module.set_data_layout(&tm.get_target_data().get_data_layout());

    // Leaf calls should remain valid after the full RS4GC pipeline too.
    rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");
    debug_verify_no_stray_calls_in_gc_functions(&module).expect("verifier should pass after rewrite");
  }

  #[test]
  fn ts_callsite_invariant_allows_leaf_calls_after_rewrite() {
    let context = Context::create();
    let module = context.create_module("passes_ts_callsite_leaf");
    let builder = context.create_builder();

    let void_ty = context.void_type();
    let i8_ty = context.i8_type();
    let gc_ptr_ty = gc::gc_ptr_type(&context);

    let callee_ty = void_ty.fn_type(&[], false);
    let callee = module.add_function("callee", callee_ty, None);

    let leaf_ty = void_ty.fn_type(&[], false);
    let leaf = module.add_function("leaf", leaf_ty, None);
    let leaf_attr = context.create_string_attribute("gc-leaf-function", "");
    leaf.add_attribute(AttributeLoc::Function, leaf_attr);

    // Name must match `is_ts_generated_function_name` so `verify_no_stray_calls_in_ts_generated_functions`
    // runs on it.
    let test_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
    let test_fn = module.add_function("__nativejs_def_deadbeef", test_ty, None);
    gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

    let obj = test_fn
      .get_first_param()
      .expect("missing obj param")
      .into_pointer_value();

    let entry = context.append_basic_block(test_fn, "entry");
    builder.position_at_end(entry);
    builder.build_load(i8_ty, obj, "pre").expect("load");
    builder.build_call(leaf, &[], "leaf").expect("call");
    builder.build_call(callee, &[], "call").expect("call");
    builder.build_load(i8_ty, obj, "post").expect("load");
    builder.build_return(None).expect("ret void");

    if let Err(err) = module.verify() {
      panic!("input module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
    }

    let tm = host_target_machine();
    module.set_triple(&tm.get_triple());
    module.set_data_layout(&tm.get_target_data().get_data_layout());

    // The call to `callee` should be rewritten into a statepoint intrinsic, while the call to
    // `leaf` remains a plain call. Both should satisfy the verifier.
    rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");
    let ir = module.print_to_string().to_string();
    assert!(
      ir.contains("call void @leaf"),
      "expected leaf call to remain a plain call, IR:\n{ir}"
    );
  }
}
