use native_js::poc::{
  demo_gc_root_derived_ptr_ir, demo_gc_root_multi_derived_ptr_ir, demo_gc_root_slots_ir,
};

#[test]
fn gc_root_slots_writeback_relocates() {
  let ir = demo_gc_root_slots_ir();

  // Allocas are in the entry block.
  assert!(ir.contains("entry:"));
  assert!(ir.contains("alloca ptr addrspace(1)"));
  assert!(ir.matches("alloca ptr addrspace(1)").count() >= 2);

  // Loads feeding `\"gc-live\"` operand bundle.
  assert!(ir.contains("%gc_live0 = load ptr addrspace(1), ptr %gc_root0"));
  assert!(ir.contains("%gc_live1 = load ptr addrspace(1), ptr %gc_root1"));
  assert!(ir.contains("[ \"gc-live\"(ptr addrspace(1) %gc_live0, ptr addrspace(1) %gc_live1) ]"));

  // `gc.relocate` results are written back to their originating slots.
  assert!(ir.contains("@llvm.experimental.gc.relocate.p1"));
  assert!(ir.contains("store ptr addrspace(1) %gc_relocate0, ptr %gc_root0"));
  assert!(ir.contains("store ptr addrspace(1) %gc_relocate1, ptr %gc_root1"));
}

#[test]
fn gc_root_derived_ptr_relocate_uses_base_index() {
  let ir = demo_gc_root_derived_ptr_ir();

  // Base + derived must both be present in a stable order (base first).
  assert!(
    ir.contains("[ \"gc-live\"(ptr addrspace(1) %gc_live0, ptr addrspace(1) %gc_live1) ]"),
    "expected gc-live to contain base then derived:\n{ir}"
  );

  // Derived pointer relocation must reference the base pointer index (0) and the derived index (1).
  assert!(
    ir.contains("i32 0, i32 1"),
    "expected gc.relocate(base_idx=0, derived_idx=1) for derived pointer:\n{ir}"
  );

  // Derived relocation should be written back to the derived slot.
  assert!(
    ir.contains("store ptr addrspace(1) %gc_relocate1, ptr %gc_root1"),
    "expected derived relocate result to be stored into derived slot:\n{ir}"
  );
}

#[test]
fn gc_root_multiple_derived_ptrs_share_base_index() {
  let ir = demo_gc_root_multi_derived_ptr_ir();

  // Stable ordering: base, derived0, derived1.
  assert!(
    ir.contains(
      "[ \"gc-live\"(ptr addrspace(1) %gc_live0, ptr addrspace(1) %gc_live1, ptr addrspace(1) %gc_live2) ]"
    ),
    "expected gc-live to contain base + both derived pointers:\n{ir}"
  );

  // Both derived relocations must reference the same base index (0).
  assert!(
    ir.contains("i32 0, i32 1") && ir.contains("i32 0, i32 2"),
    "expected derived relocates to reuse base_idx=0:\n{ir}"
  );
}
