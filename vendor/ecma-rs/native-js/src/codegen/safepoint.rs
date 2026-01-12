//! Explicit GC safepoint polling helpers.
//!
//! ## Why this exists
//! LLVM's statepoint-based GC needs *safepoints* where the runtime can stop the
//! world and relocate live GC pointers. Natural safepoints are function calls,
//! but AOT-compiled code can have tight loops with no calls.
//!
//! To ensure such loops remain interruptible, `native-js` inserts an explicit
//! backedge poll.
//!
//! ## Policy (initial)
//! * Insert a poll on **every loop backedge**.
//! * Fast path: inline the runtime's exported epoch (`@RT_GC_EPOCH`) and branch
//!   on the low bit.
//! * Slow path: if requested, call `rt_gc_safepoint_slow(epoch)` via a **normal
//!   callsite**, which is then rewritten into an LLVM statepoint by
//!   `rewrite-statepoints-for-gc`.
//!
//! This keeps the runtime ABI stable while making it easy to tune overhead later
//! (e.g. poll every N iterations, or only for proven-long loops).

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::values::AsValueRef;
use inkwell::values::BasicValue;
use inkwell::values::{FunctionValue, IntValue};
use inkwell::IntPredicate;
use llvm_sys::core::{
  LLVMGetIntTypeWidth, LLVMGetPointerAddressSpace, LLVMGetTypeKind, LLVMGlobalGetValueType, LLVMIsGlobalConstant,
  LLVMIsThreadLocal, LLVMSetAlignment, LLVMSetOrdering, LLVMTypeOf,
};
use llvm_sys::{LLVMAtomicOrdering, LLVMTypeKind};

use crate::runtime_abi::{RuntimeAbi, RuntimeFn};

fn get_or_declare_rt_gc_epoch<'ctx>(
  context: &'ctx Context,
  module: &Module<'ctx>,
) -> inkwell::values::GlobalValue<'ctx> {
  if let Some(existing) = module.get_global("RT_GC_EPOCH") {
    unsafe {
      let addrspace = LLVMGetPointerAddressSpace(LLVMTypeOf(existing.as_value_ref()));
      if addrspace != 0 {
        panic!("RT_GC_EPOCH must be in addrspace(0)");
      }
      if LLVMIsThreadLocal(existing.as_value_ref()) != 0 {
        panic!("RT_GC_EPOCH must not be thread-local");
      }
      let ty = LLVMGlobalGetValueType(existing.as_value_ref());
      if LLVMGetTypeKind(ty) != LLVMTypeKind::LLVMIntegerTypeKind || LLVMGetIntTypeWidth(ty) != 64 {
        panic!("RT_GC_EPOCH must have type i64");
      }
      if LLVMIsGlobalConstant(existing.as_value_ref()) != 0 {
        panic!("RT_GC_EPOCH must be a mutable global");
      }
    }
    return existing;
  }
  // Declared in `runtime-native/include/runtime_native.h`.
  //
  // The runtime defines the symbol; native-js only needs an external declaration
  // so it can emit an atomic load.
  module.add_global(context.i64_type(), None, "RT_GC_EPOCH")
}

/// Emit a GC poll at the current insertion point (intended for loop backedges).
///
/// After this returns, the builder is positioned at the continuation block
/// (`gc.poll.cont`) so the caller can finish the backedge (e.g. branch to the
/// loop header).
pub fn emit_backedge_gc_poll<'ctx>(
  context: &'ctx Context,
  module: &Module<'ctx>,
  builder: &Builder<'ctx>,
  current_fn: FunctionValue<'ctx>,
) -> IntValue<'ctx> {
  let i64_ty = context.i64_type();
  let rt_gc_epoch = get_or_declare_rt_gc_epoch(context, module);

  let epoch = builder
    .build_load(i64_ty, rt_gc_epoch.as_pointer_value(), "gc.epoch")
    .expect("build RT_GC_EPOCH load")
    .into_int_value();

  // Treat the epoch as `_Atomic uint64_t` and use Acquire ordering.
  //
  // Note: inkwell's atomic helpers are limited, so we directly set ordering on
  // the produced `load` instruction via the LLVM C API.
  if let Some(load_inst) = epoch.as_instruction_value() {
    unsafe {
      LLVMSetOrdering(load_inst.as_value_ref(), LLVMAtomicOrdering::LLVMAtomicOrderingAcquire);
      // Atomic loads must be sufficiently aligned for the target.
      //
      // In practice our poll global is a `u64` (`_Atomic uint64_t` in
      // `runtime_native.h`), so it is always 8-byte aligned on supported targets.
      //
      // Without an explicit `target datalayout`, LLVM can conservatively print
      // smaller alignments (e.g. `align 4`), which can lead to invalid or
      // miscompiled atomic loads under MCJIT/ORC.
      LLVMSetAlignment(load_inst.as_value_ref(), 8);
    }
  }

  // requested = (epoch & 1) != 0
  let lowbit = builder
    .build_and(epoch, i64_ty.const_int(1, false), "gc.epoch.lowbit")
    .expect("build epoch lowbit");
  let requested = builder
    .build_int_compare(IntPredicate::NE, lowbit, i64_ty.const_zero(), "gc.poll.requested")
    .expect("build poll compare");

  let slow_block = context.append_basic_block(current_fn, "gc.poll.slow");
  let cont_block = context.append_basic_block(current_fn, "gc.poll.cont");
  builder
    .build_conditional_branch(requested, slow_block, cont_block)
    .expect("build poll branch");

  builder.position_at_end(slow_block);
  let rt = RuntimeAbi::new(context, module);
  let _ = rt
    .emit_runtime_call(
      builder,
      RuntimeFn::GcSafepointSlow,
      &[epoch.into()],
      "gc.safepoint",
    )
    .expect("emit rt_gc_safepoint_slow call");
  builder
    .build_unconditional_branch(cont_block)
    .expect("branch to poll continuation");

  builder.position_at_end(cont_block);

  // NOTE: `rewrite-statepoints-for-gc` will insert `gc.relocate` and rewrite uses
  // of live `ptr addrspace(1)` values across the safepoint call.
  requested
}
