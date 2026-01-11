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
use crate::runtime_fn::GcEffect;
use llvm_sys::core::{
  LLVMBuildAlloca, LLVMBuildCall2, LLVMBuildLoad2, LLVMBuildStore, LLVMCreateBuilderInContext,
  LLVMDisposeBuilder, LLVMGetFirstInstruction, LLVMGetPointerAddressSpace, LLVMGetReturnType,
  LLVMGetStringAttributeAtIndex, LLVMGetTypeKind, LLVMGlobalGetValueType, LLVMIsAFunction,
  LLVMPointerType, LLVMPositionBuilderAtEnd, LLVMPositionBuilderBefore, LLVMTypeOf,
  LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{
  LLVMBasicBlockRef, LLVMBuilderRef, LLVMContextRef, LLVMTypeRef, LLVMValueRef,
};
use llvm_sys::LLVMTypeKind;
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

/// Treating a call as a "safepoint" is only required if the callee may trigger GC.
///
/// LLVM's `rewrite-statepoints-for-gc` pass uses a function attribute named
/// `"gc-leaf-function"` to mark callees that are known to not contain safepoints.
///
/// We mirror that convention for *manually emitted* compiled calls: if a direct
/// callee has `"gc-leaf-function"`, we treat it as [`GcEffect::NoGc`].
unsafe fn direct_callee_gc_effect_from_attrs(callee: LLVMValueRef) -> Option<GcEffect> {
  // Only direct/global callees can carry attributes. If this isn't a function
  // value, it's an indirect call and we can't infer anything.
  if LLVMIsAFunction(callee).is_null() {
    return None;
  }

  // Attribute keys in the C API are length-delimited, but must be NUL-terminated.
  const KEY: &[u8] = b"gc-leaf-function\0";
  let attr = LLVMGetStringAttributeAtIndex(
    callee,
    llvm_sys::LLVMAttributeFunctionIndex,
    KEY.as_ptr().cast(),
    (KEY.len() - 1) as u32,
  );
  (!attr.is_null()).then_some(GcEffect::NoGc)
}

pub struct GcFrame {
  gc_ptr_ty: LLVMTypeRef,
  entry_block: LLVMBasicBlockRef,
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
    // Ensure allocas dominate all uses, even if roots are created from within nested blocks.
    //
    // Inserting allocas at the end of the entry block is *usually* fine, but can produce invalid IR
    // if a caller tries to root a value while the main builder is positioned earlier in the entry
    // block (store would occur before alloca). Prefer inserting before the entry's first
    // instruction, which guarantees dominance.
    let first = LLVMGetFirstInstruction(entry_block);
    if first.is_null() {
      LLVMPositionBuilderAtEnd(alloca_builder, entry_block);
    } else {
      LLVMPositionBuilderBefore(alloca_builder, first);
    }

    let gc_ptr_ty = LLVMPointerType(LLVMVoidTypeInContext(ctx), 1);

    Self {
      gc_ptr_ty,
      entry_block,
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
    // Place allocas at the start of the entry block so they dominate all uses, regardless of where
    // roots are created from (including from within nested blocks) and regardless of how the
    // caller positions its main builder within the entry block.
    //
    // This is important even if the entry block was empty at `GcFrame::new` time: other codegen may
    // later insert instructions into the entry, and the alloca builder's insertion point would
    // otherwise drift to the end of the block, producing `store` instructions that can precede the
    // alloca in the same block (invalid IR).
    let first = LLVMGetFirstInstruction(self.entry_block);
    if first.is_null() {
      LLVMPositionBuilderAtEnd(self.alloca_builder, self.entry_block);
    } else {
      LLVMPositionBuilderBefore(self.alloca_builder, first);
    }

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

  unsafe fn safepoint_call_inner(
    &self,
    builder: LLVMBuilderRef,
    statepoints: &mut StatepointEmitter,
    callee_ptr: LLVMValueRef,
    callee_fn_ty: LLVMTypeRef,
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

    let mut base_indices = Vec::with_capacity(gc_live_slots.len() + call_args.len());
    for (idx, _) in base_slots.iter().enumerate() {
      base_indices.push(idx as u32);
    }
    for &(base, _) in &derived_roots {
      let base_idx = *base_slot_index.get(&base).expect(
        "derived root references base slot that is not rooted as a base (root_base must be called first)",
      );
      base_indices.push(base_idx);
    }

    let num_rooted = gc_live_slots.len();
    let mut live_vals = Vec::with_capacity(num_rooted + call_args.len());
    for (idx, slot) in gc_live_slots.iter().copied().enumerate() {
      live_vals.push(self.load(builder, slot, &format!("gc_live{idx}")));
    }

    // Auto-include any `ptr addrspace(1)` call arguments in the `"gc-live"` bundle.
    //
    // These may be *outgoing arguments* that the callee will read, so they must be tracked and
    // relocatable at the statepoint even if the caller doesn't use them after the call.
    for &arg in call_args {
      let ty = LLVMTypeOf(arg);
      if LLVMGetTypeKind(ty) == LLVMTypeKind::LLVMPointerTypeKind
        && LLVMGetPointerAddressSpace(ty) == 1
      {
        let derived_idx = live_vals.len() as u32;
        live_vals.push(arg);
        base_indices.push(derived_idx);
      }
    }

    let sp = statepoints.emit_statepoint_call_indirect(
      builder,
      callee_ptr,
      callee_fn_ty,
      call_args,
      &live_vals,
      &base_indices,
    );

    // Write back relocated values for rooted slots only. Call arguments appended to `live_vals`
    // do not have backing slots; their relocation is handled by the statepoint lowering itself.
    for (idx, slot) in gc_live_slots.into_iter().enumerate() {
      self.store(builder, slot, sp.relocated[idx]);
    }

    sp.result
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
    let callee_fn_ty = LLVMGlobalGetValueType(callee);
    self.safepoint_call_inner(builder, statepoints, callee, callee_fn_ty, call_args)
  }

  /// Like [`GcFrame::safepoint_call`], but supports an **indirect** callee (`ptr %fp`).
  ///
  /// `callee_fn_ty` must be the callee's function type (not a pointer type); it is used to attach
  /// the required `elementtype(<fn-ty>)` attribute to the statepoint callee operand under LLVM 18
  /// opaque pointers.
  pub unsafe fn safepoint_call_indirect(
    &self,
    builder: LLVMBuilderRef,
    statepoints: &mut StatepointEmitter,
    callee_ptr: LLVMValueRef,
    callee_fn_ty: LLVMTypeRef,
    call_args: &[LLVMValueRef],
  ) -> Option<LLVMValueRef> {
    self.safepoint_call_inner(builder, statepoints, callee_ptr, callee_fn_ty, call_args)
  }

  /// Emit a call to a compiled/user function, choosing between a plain call and a statepointed call.
  ///
  /// **Conservative default:** if `effect` is `None`, this assumes the callee is `may-GC` and emits a
  /// statepoint. This ensures callers have stackmap records at the return address when GC can run
  /// inside the callee.
  pub unsafe fn compiled_call(
    &self,
    builder: LLVMBuilderRef,
    statepoints: &mut StatepointEmitter,
    callee: LLVMValueRef,
    call_args: &[LLVMValueRef],
    effect: Option<GcEffect>,
  ) -> Option<LLVMValueRef> {
    let callee_fn_ty = LLVMGlobalGetValueType(callee);
    self.compiled_call_indirect(builder, statepoints, callee, callee_fn_ty, call_args, effect)
  }

  /// Like [`GcFrame::compiled_call`], but supports an indirect callee value.
  pub unsafe fn compiled_call_indirect(
    &self,
    builder: LLVMBuilderRef,
    statepoints: &mut StatepointEmitter,
    callee_ptr: LLVMValueRef,
    callee_fn_ty: LLVMTypeRef,
    call_args: &[LLVMValueRef],
    effect: Option<GcEffect>,
  ) -> Option<LLVMValueRef> {
    let effect = effect.unwrap_or_else(|| {
      // Conservative default: assume the call may GC unless explicitly annotated
      // as a leaf/no-GC callee.
      direct_callee_gc_effect_from_attrs(callee_ptr).unwrap_or(GcEffect::MayGc)
    });

    match effect {
      GcEffect::NoGc => {
        let ret_ty = LLVMGetReturnType(callee_fn_ty);
        let is_void = LLVMGetTypeKind(ret_ty) == LLVMTypeKind::LLVMVoidTypeKind;
        // Void-typed instructions must not have SSA names (`%x = call void ...` is invalid).
        let call_name = if is_void {
          b"\0".as_ptr().cast()
        } else {
          b"call\0".as_ptr().cast()
        };
        let call = LLVMBuildCall2(
          builder,
          callee_fn_ty,
          callee_ptr,
          call_args.as_ptr().cast_mut(),
          call_args.len() as u32,
          call_name,
        );
        if is_void {
          None
        } else {
          Some(call)
        }
      }
      GcEffect::MayGc => self.safepoint_call_inner(builder, statepoints, callee_ptr, callee_fn_ty, call_args),
    }
  }
}

impl Drop for GcFrame {
  fn drop(&mut self) {
    unsafe {
      LLVMDisposeBuilder(self.alloca_builder);
    }
  }
}
