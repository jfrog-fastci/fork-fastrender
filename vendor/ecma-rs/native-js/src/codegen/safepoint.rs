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
//! * Fast path: a cheap leaf check (`rt_gc_poll() -> i1`) that is **not**
//!   rewritten into a statepoint.
//! * Slow path: if requested, call into the runtime via a **normal callsite**
//!   (`rt_gc_safepoint_gc()`), which is then rewritten into an LLVM statepoint by
//!   `rewrite-statepoints-for-gc`.
//!
//! This keeps the runtime ABI stable while making it easy to tune overhead later
//! (e.g. poll every N iterations, or only for proven-long loops).

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::values::{FunctionValue, IntValue};

/// Ensure `declare i1 @rt_gc_poll() "gc-leaf-function"` exists in `module`.
///
/// Marking the poll as a *GC leaf* ensures LLVM's `rewrite-statepoints-for-gc`
/// pass does not wrap it in a statepoint (the poll itself is just a cheap check).
pub fn get_or_declare_rt_gc_poll<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> FunctionValue<'ctx> {
  if let Some(existing) = module.get_function("rt_gc_poll") {
    return existing;
  }

  let fn_ty = context.bool_type().fn_type(&[], false);
  let func = module.add_function("rt_gc_poll", fn_ty, None);

  // A string attribute with an empty value prints as `"gc-leaf-function"` in IR.
  let leaf = context.create_string_attribute("gc-leaf-function", "");
  func.add_attribute(inkwell::attributes::AttributeLoc::Function, leaf);

  func
}

/// Emit a GC poll at the current insertion point (intended for loop backedges).
///
/// After this returns, the builder is positioned at the continuation block
/// (`gc.poll.cont`) so the caller can finish the backedge (e.g. branch to the
/// loop header).
///
/// `rt_gc_safepoint_gc` should be the addrspace(1) wrapper from
/// [`crate::runtime_abi::RuntimeAbi`]. The call is emitted as a normal call; it
/// becomes a statepoint once `rewrite-statepoints-for-gc` runs.
pub fn emit_backedge_gc_poll<'ctx>(
  context: &'ctx Context,
  module: &Module<'ctx>,
  builder: &Builder<'ctx>,
  current_fn: FunctionValue<'ctx>,
  rt_gc_safepoint_gc: FunctionValue<'ctx>,
) -> IntValue<'ctx> {
  let rt_gc_poll = get_or_declare_rt_gc_poll(context, module);

  let requested = builder
    .build_call(rt_gc_poll, &[], "gc.poll.requested")
    .expect("build rt_gc_poll call")
    .try_as_basic_value()
    .left()
    .expect("rt_gc_poll returns i1")
    .into_int_value();

  let slow_block = context.append_basic_block(current_fn, "gc.poll.slow");
  let cont_block = context.append_basic_block(current_fn, "gc.poll.cont");
  builder
    .build_conditional_branch(requested, slow_block, cont_block)
    .expect("build poll branch");

  builder.position_at_end(slow_block);
  let _ = builder
    .build_call(rt_gc_safepoint_gc, &[], "gc.safepoint")
    .expect("build rt_gc_safepoint_gc call");
  builder
    .build_unconditional_branch(cont_block)
    .expect("branch to poll continuation");

  builder.position_at_end(cont_block);

  // NOTE: `rewrite-statepoints-for-gc` will insert `gc.relocate` and rewrite uses
  // of live `ptr addrspace(1)` values across the safepoint call.
  requested
}
