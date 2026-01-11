use llvm_sys::prelude::{LLVMBuilderRef, LLVMValueRef};

use crate::gc::statepoint::StatepointEmitter;

/// Emits a loop backedge safepoint poll via a **void** statepointed call.
///
/// This is intended for runtime entrypoints like `rt_gc_safepoint()` which return `void` but still
/// require relocation of GC pointers that are live across the poll.
pub unsafe fn emit_backedge_safepoint_poll(
  statepoints: &mut StatepointEmitter,
  builder: LLVMBuilderRef,
  rt_gc_safepoint: LLVMValueRef,
  live_gc_ptrs: &[LLVMValueRef],
) -> Vec<LLVMValueRef> {
  statepoints.emit_statepoint_call_void(builder, rt_gc_safepoint, &[], live_gc_ptrs)
}
