use native_js::poc::demo_gc_root_slots_ir;

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

