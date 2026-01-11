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
//!
//! ## Derived / interior pointers
//!
//! Real code often forms **interior pointers** (e.g. `getelementptr` into an object)
//! and keeps them live across safepoints. LLVM models this via *base+derived*
//! relocation:
//!
//! `gc.relocate(token, base_idx, derived_idx)`
//!
//! - `base_idx` points at the base object pointer in the `"gc-live"` bundle.
//! - `derived_idx` points at the derived (interior) pointer value.
//!
//! If you can cheaply recompute the derived pointer after the safepoint, prefer
//! rooting only the base pointer and re-doing the `gep` from the relocated base.
//! Root a derived pointer only when the offset cannot be reconstructed later.

use crate::gc::statepoint::StatepointEmitter;
use llvm_sys::core::{
  LLVMBuildAlloca, LLVMBuildLoad2, LLVMBuildStore, LLVMCreateBuilderInContext, LLVMDisposeBuilder,
  LLVMPointerType, LLVMPositionBuilderAtEnd, LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{LLVMBasicBlockRef, LLVMBuilderRef, LLVMContextRef, LLVMTypeRef, LLVMValueRef};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CString;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct GcSlot {
  alloca: LLVMValueRef,
}

impl GcSlot {
  pub(crate) fn as_alloca(self) -> LLVMValueRef {
    self.alloca
  }
}

#[derive(Clone, Copy, Debug)]
pub enum GcRoot {
  Base(GcSlot),
  Derived { base: GcSlot, derived: GcSlot },
}

pub struct GcFrame {
  gc_ptr_ty: LLVMTypeRef,
  alloca_builder: LLVMBuilderRef,
  rooted: RefCell<Vec<GcRoot>>,
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

  /// Allocate a new stack slot (`alloca`) of GC pointer type and store `init` into it.
  ///
  /// The `alloca` itself is placed in the function entry block (via the frame's
  /// dedicated alloca builder) while the initializing store happens at the
  /// caller's current insertion point.
  unsafe fn alloc_slot_untracked(&self, builder: LLVMBuilderRef, init: LLVMValueRef) -> GcSlot {
    let slot_id = self.next_slot_id.get();
    self.next_slot_id.set(slot_id + 1);
    let slot_name = CString::new(format!("gc_root{slot_id}")).unwrap();

    let alloca = LLVMBuildAlloca(self.alloca_builder, self.gc_ptr_ty, slot_name.as_ptr());
    LLVMBuildStore(builder, init, alloca);

    GcSlot { alloca }
  }

  /// Root a base GC pointer.
  pub unsafe fn root_base(&self, builder: LLVMBuilderRef, ptr: LLVMValueRef) -> GcSlot {
    let slot = self.alloc_slot_untracked(builder, ptr);
    self.rooted.borrow_mut().push(GcRoot::Base(slot));
    slot
  }

  /// Back-compat alias for [`GcFrame::root_base`].
  pub unsafe fn alloc_slot(&self, builder: LLVMBuilderRef, init: LLVMValueRef) -> GcSlot {
    self.root_base(builder, init)
  }

  /// Root an interior pointer (`derived`) along with its base object pointer (`base`).
  ///
  /// The base must be rooted separately (via [`GcFrame::root_base`]) so it appears
  /// in the `"gc-live"` bundle and can be referenced by index.
  pub unsafe fn root_derived(
    &self,
    builder: LLVMBuilderRef,
    base: &GcSlot,
    derived: LLVMValueRef,
  ) -> GcSlot {
    let derived_slot = self.alloc_slot_untracked(builder, derived);
    self
      .rooted
      .borrow_mut()
      .push(GcRoot::Derived { base: *base, derived: derived_slot });
    derived_slot
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
    let roots: Vec<GcRoot> = self.rooted.borrow().iter().copied().collect();

    // Deterministic ordering: all bases first (in insertion order), then derived
    // pointers (in insertion order).
    let mut base_slots = Vec::new();
    let mut derived_roots: Vec<(GcSlot, GcSlot)> = Vec::new();
    for root in roots {
      match root {
        GcRoot::Base(slot) => base_slots.push(slot),
        GcRoot::Derived { base, derived } => derived_roots.push((base, derived)),
      }
    }

    let mut base_slot_index: HashMap<GcSlot, u32> = HashMap::with_capacity(base_slots.len());
    for (idx, slot) in base_slots.iter().copied().enumerate() {
      base_slot_index.insert(slot, idx as u32);
    }

    let mut gc_live_slots = Vec::with_capacity(base_slots.len() + derived_roots.len());
    gc_live_slots.extend_from_slice(&base_slots);
    for &(_, derived) in &derived_roots {
      gc_live_slots.push(derived);
    }

    let mut base_indices = Vec::with_capacity(gc_live_slots.len());
    for (idx, _) in base_slots.iter().enumerate() {
      base_indices.push(idx as u32);
    }
    for &(base, _) in &derived_roots {
      let base_idx = *base_slot_index.get(&base).expect(
        "derived root references base slot that is not rooted as a base (root_base must be called first)",
      );
      base_indices.push(base_idx);
    }

    let mut live_vals = Vec::with_capacity(gc_live_slots.len());
    for (idx, slot) in gc_live_slots.iter().copied().enumerate() {
      live_vals.push(self.load(builder, slot, &format!("gc_live{idx}")));
    }

    let sp = statepoints.emit_statepoint_call_with_base_indices(
      builder,
      callee,
      call_args,
      &live_vals,
      &base_indices,
    );

    for (slot, relocated) in gc_live_slots.into_iter().zip(sp.relocated.iter().copied()) {
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
