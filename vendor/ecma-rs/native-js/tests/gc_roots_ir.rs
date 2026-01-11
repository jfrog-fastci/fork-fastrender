use native_js::poc::{
  demo_compiled_call_nogc_void_ir, demo_gc_root_alloca_dominance_ir,
  demo_gc_root_derived_ptr_ir, demo_gc_root_multi_derived_ptr_ir, demo_gc_root_slots_indirect_call_ir,
  demo_gc_root_slots_ir,
};

fn parse_relocate_indices(line: &str) -> Option<(u32, u32)> {
  if !line.contains("@llvm.experimental.gc.relocate.p1") {
    return None;
  }

  // call ... @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)
  let mut parts = line.split(',');
  let _tok = parts.next()?;
  let base_part = parts.next()?.trim();
  let derived_part = parts.next()?.trim();

  fn parse_i32_constant(s: &str) -> Option<u32> {
    let mut it = s.split_whitespace();
    let _ty = it.next()?;
    let value = it.next()?.trim_end_matches(')');
    value.parse().ok()
  }

  Some((parse_i32_constant(base_part)?, parse_i32_constant(derived_part)?))
}

fn parse_relocates(ir: &str) -> Vec<(String, u32, u32)> {
  ir.lines()
    .filter(|l| l.contains("@llvm.experimental.gc.relocate.p1"))
    .filter_map(|l| {
      let (lhs, _) = l.split_once('=')?;
      let (base, derived) = parse_relocate_indices(l)?;
      Some((lhs.trim().to_string(), base, derived))
    })
    .collect()
}

#[test]
fn gc_root_slots_writeback_relocates() {
  let ir = demo_gc_root_slots_ir();

  // `native-js` uses the same default patchpoint ID as LLVM's `rewrite-statepoints-for-gc` pass
  // (0xABCDEF00 / 2882400000) so `runtime-native` can cheaply identify statepoint records in debug
  // verification mode.
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint.p0(i64 2882400000, i32 0"),
    "expected default statepoint patchpoint ID in IR:\n{ir}"
  );

  // Allocas are in the entry block.
  assert!(ir.contains("entry:"));
  assert!(ir.contains("alloca ptr addrspace(1)"));
  assert!(ir.matches("alloca ptr addrspace(1)").count() >= 2);

  // Loads feeding `"gc-live"` operand bundle.
  assert!(ir.contains("%gc_live0 = load ptr addrspace(1), ptr %gc_root0"));
  assert!(ir.contains("%gc_live1 = load ptr addrspace(1), ptr %gc_root1"));
  assert!(ir.contains("[ \"gc-live\"(ptr addrspace(1) %gc_live0, ptr addrspace(1) %gc_live1) ]"));

  let relocates = parse_relocates(&ir);
  assert_eq!(relocates.len(), 2, "expected 2 gc.relocate calls:\n{ir}");

  // Base roots relocate (base_idx == derived_idx) and are written back to their slots.
  for (slot_name, idx) in [("%gc_root0", 0u32), ("%gc_root1", 1u32)] {
    let (ssa, _, _) = relocates
      .iter()
      .find(|(_, base, derived)| *base == idx && *derived == idx)
      .unwrap_or_else(|| panic!("missing relocate({idx},{idx}) in IR:\n{ir}"));
    assert!(
      ir.contains(&format!("store ptr addrspace(1) {ssa}, ptr {slot_name}")),
      "expected relocate result {ssa} to be stored back into {slot_name}:\n{ir}"
    );
  }
}

#[test]
fn gc_root_alloca_dominates_entry_store() {
  let ir = demo_gc_root_alloca_dominance_ir();

  assert!(
    ir.contains("alloca ptr addrspace(1)"),
    "expected rooted slot alloca in IR:\n{ir}"
  );
  assert!(
    ir.contains("store ptr addrspace(1) null, ptr %gc_root0"),
    "expected root slot store in IR:\n{ir}"
  );
  assert!(
    ir.contains("call void @callee"),
    "expected fixture to contain the dummy call used to stress builder placement:\n{ir}"
  );
}

#[test]
fn compiled_call_nogc_void_emits_unnamed_void_call() {
  let ir = demo_compiled_call_nogc_void_ir();

  assert!(
    ir.contains("call void @callee()"),
    "expected a direct void call in IR:\n{ir}"
  );
  assert!(
    !ir.contains("= call void @callee()"),
    "void calls must not have SSA names:\n{ir}"
  );
}

#[test]
fn gc_statepoint_indirect_call_has_elementtype() {
  let ir = demo_gc_root_slots_indirect_call_ir();

  assert!(
    ir.contains("llvm.experimental.gc.statepoint.p0"),
    "expected statepoint intrinsic in IR:\n{ir}"
  );

  // Indirect callees must be annotated with `elementtype(<fn-ty>)` under opaque pointers.
  assert!(
    ir.contains("ptr elementtype(void ()) %"),
    "expected statepoint callee operand to include elementtype(void ()) for indirect call:\n{ir}"
  );

  assert!(
    ir.contains("\"gc-live\""),
    "expected gc-live bundle in statepoint call:\n{ir}"
  );
}

#[test]
fn gc_root_derived_ptr_relocate_uses_base_index() {
  let ir = demo_gc_root_derived_ptr_ir();

  // Base + derived must both be present in a stable order (base first).
  assert!(
    ir.contains("[ \"gc-live\"(ptr addrspace(1) %gc_live0, ptr addrspace(1) %gc_live1) ]"),
    "expected gc-live to contain base then derived:\n{ir}"
  );

  let relocates = parse_relocates(&ir);
  assert_eq!(relocates.len(), 2, "expected 2 gc.relocate calls:\n{ir}");

  let base_ssa = relocates
    .iter()
    .find(|(_, base, derived)| *base == 0 && *derived == 0)
    .unwrap_or_else(|| panic!("missing relocate(0,0) for base:\n{ir}"))
    .0
    .clone();
  let derived_ssa = relocates
    .iter()
    .find(|(_, base, derived)| *base == 0 && *derived == 1)
    .unwrap_or_else(|| panic!("missing relocate(0,1) for derived:\n{ir}"))
    .0
    .clone();

  // Relocation results must be written back into the correct slots.
  assert!(
    ir.contains(&format!("store ptr addrspace(1) {base_ssa}, ptr %gc_root0")),
    "expected base relocate result to be stored into base slot:\n{ir}"
  );
  assert!(
    ir.contains(&format!("store ptr addrspace(1) {derived_ssa}, ptr %gc_root1")),
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

  let relocates = parse_relocates(&ir);
  assert_eq!(relocates.len(), 3, "expected 3 gc.relocate calls:\n{ir}");

  let base_ssa = relocates
    .iter()
    .find(|(_, base, derived)| *base == 0 && *derived == 0)
    .unwrap_or_else(|| panic!("missing relocate(0,0) for base:\n{ir}"))
    .0
    .clone();
  let d0_ssa = relocates
    .iter()
    .find(|(_, base, derived)| *base == 0 && *derived == 1)
    .unwrap_or_else(|| panic!("missing relocate(0,1) for derived0:\n{ir}"))
    .0
    .clone();
  let d1_ssa = relocates
    .iter()
    .find(|(_, base, derived)| *base == 0 && *derived == 2)
    .unwrap_or_else(|| panic!("missing relocate(0,2) for derived1:\n{ir}"))
    .0
    .clone();

  assert!(
    ir.contains(&format!("store ptr addrspace(1) {base_ssa}, ptr %gc_root0")),
    "expected base relocate result to be stored into base slot:\n{ir}"
  );
  assert!(
    ir.contains(&format!("store ptr addrspace(1) {d0_ssa}, ptr %gc_root1")),
    "expected derived0 relocate result to be stored into derived0 slot:\n{ir}"
  );
  assert!(
    ir.contains(&format!("store ptr addrspace(1) {d1_ssa}, ptr %gc_root2")),
    "expected derived1 relocate result to be stored into derived1 slot:\n{ir}"
  );
}
