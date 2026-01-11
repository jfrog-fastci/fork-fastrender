//! GC root slot management.
//!
//! ## Why slots?
//!
//! LLVM statepoints (`gc.statepoint`/`gc.relocate`) require that every GC pointer
//! live across a safepoint has its post-GC ("relocated") value used for all
//! subsequent uses.
//!
//! Doing this purely in SSA requires rewriting uses to thread relocated values
//! through the CFG, which is complex to do by hand during early bring-up.
//!
//! A practical PoC strategy is to represent each GC local as an explicit stack
//! slot (`alloca ptr addrspace(1)`) and:
//!   1. load all rooted slots for the `"gc-live"` operand bundle at each
//!      safepoint,
//!   2. emit `gc.relocate` for each live pointer, and
//!   3. store the relocated values back into their originating slots.
//!
//! This is correct and straightforward, at the cost of extra memory traffic
//! (loads/stores can inhibit some optimizations). Later we can:
//!   - add caching to avoid redundant reloads where safe, or
//!   - switch to an LLVM pass-based SSA rewriting strategy.

use crate::gc::statepoint::StatepointEmitter;
use llvm_sys::core::{
  LLVMBuildAlloca, LLVMBuildLoad2, LLVMBuildStore, LLVMCreateBuilderInContext, LLVMDisposeBuilder,
  LLVMPointerType, LLVMPositionBuilderAtEnd, LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{LLVMBasicBlockRef, LLVMBuilderRef, LLVMContextRef, LLVMTypeRef, LLVMValueRef};
use std::cell::Cell;
use std::ffi::CString;
use std::cell::RefCell;

#[derive(Clone, Copy)]
pub struct GcSlot {
  alloca: LLVMValueRef,
}

impl GcSlot {
  pub(crate) fn as_alloca(self) -> LLVMValueRef {
    self.alloca
  }
}

pub struct GcFrame {
  gc_ptr_ty: LLVMTypeRef,
  alloca_builder: LLVMBuilderRef,
  rooted: RefCell<Vec<GcSlot>>,
  next_slot_id: Cell<usize>,
}

pub struct RootScope<'a> {
  frame: &'a GcFrame,
  rooted_len: usize,
}

impl Drop for RootScope<'_> {
  fn drop(&mut self) {
    self.frame.rooted.borrow_mut().truncate(self.rooted_len);
  }
}

impl GcFrame {
  pub unsafe fn new(ctx: LLVMContextRef, entry_block: LLVMBasicBlockRef) -> Self {
    let alloca_builder = LLVMCreateBuilderInContext(ctx);
    LLVMPositionBuilderAtEnd(alloca_builder, entry_block);

    let gc_ptr_ty = LLVMPointerType(LLVMVoidTypeInContext(ctx), 1);

    Self {
      gc_ptr_ty,
      alloca_builder,
      rooted: RefCell::new(Vec::new()),
      next_slot_id: Cell::new(0),
    }
  }

  pub fn gc_ptr_ty(&self) -> LLVMTypeRef {
    self.gc_ptr_ty
  }

  /// Create a new rooted slot and store `init` into it.
  ///
  /// The `alloca` itself is placed in the function entry block (via the frame's
  /// dedicated alloca builder) while the initializing store happens at the
  /// caller's current insertion point.
  pub unsafe fn alloc_slot(&self, builder: LLVMBuilderRef, init: LLVMValueRef) -> GcSlot {
    let slot_id = self.next_slot_id.get();
    self.next_slot_id.set(slot_id + 1);
    let slot_name = CString::new(format!("gc_root{slot_id}")).unwrap();

    let alloca = LLVMBuildAlloca(self.alloca_builder, self.gc_ptr_ty, slot_name.as_ptr());
    LLVMBuildStore(builder, init, alloca);

    let slot = GcSlot { alloca };
    self.rooted.borrow_mut().push(slot);
    slot
  }

  pub unsafe fn load(&self, builder: LLVMBuilderRef, slot: GcSlot, name: &str) -> LLVMValueRef {
    let name = CString::new(name).unwrap();
    LLVMBuildLoad2(builder, self.gc_ptr_ty, slot.as_alloca(), name.as_ptr())
  }

  pub unsafe fn store(&self, builder: LLVMBuilderRef, slot: GcSlot, val: LLVMValueRef) {
    LLVMBuildStore(builder, val, slot.as_alloca());
  }

  pub fn scope(&self) -> RootScope<'_> {
    let rooted_len = self.rooted.borrow().len();
    RootScope { frame: self, rooted_len }
  }

  pub unsafe fn with_rooted_temp<T>(
    &self,
    builder: LLVMBuilderRef,
    init: LLVMValueRef,
    f: impl FnOnce(GcSlot) -> T,
  ) -> T {
    let scope = self.scope();
    let slot = self.alloc_slot(builder, init);
    let out = f(slot);
    drop(scope);
    out
  }

  /// Emit a safepointed call and write relocated values back into all rooted
  /// slots.
  pub unsafe fn safepoint_call(
    &self,
    builder: LLVMBuilderRef,
    statepoints: &mut StatepointEmitter,
    callee: LLVMValueRef,
    call_args: &[LLVMValueRef],
  ) -> Option<LLVMValueRef> {
    let rooted_slots: Vec<GcSlot> = self.rooted.borrow().iter().copied().collect();

    let mut live_vals = Vec::with_capacity(rooted_slots.len());
    for (idx, slot) in rooted_slots.iter().copied().enumerate() {
      live_vals.push(self.load(builder, slot, &format!("gc_live{idx}")));
    }

    let sp = statepoints.emit_statepoint_call(builder, callee, call_args, &live_vals);

    for (slot, relocated) in rooted_slots.into_iter().zip(sp.relocated.iter().copied()) {
      self.store(builder, slot, relocated);
    }

    sp.result
  }
}

impl Drop for GcFrame {
  fn drop(&mut self) {
    unsafe {
      LLVMDisposeBuilder(self.alloca_builder);
    }
  }
}
