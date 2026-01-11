#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::toolchain::{LlvmToolchain, OptLevel};
use object::{Object, ObjectSection};
use runtime_native::stackmaps::Location;
use runtime_native::stackmaps::StackMap;
use runtime_native::statepoints::{StatepointRecord, LLVM18_STATEPOINT_HEADER_CONSTANTS};
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

fn assert_gc_root_locations(
  stackmap: &StackMap,
  expected_roots_hist: Option<BTreeMap<usize, usize>>,
) {
  let mut gc_locs = 0usize;
  let mut sp_records = 0usize;
  let mut roots_hist: BTreeMap<usize, usize> = BTreeMap::new();

  for rec in &stackmap.records {
    let Ok(sp) = StatepointRecord::new(rec) else {
      // Not a statepoint record (could be a patchpoint); ignore.
      continue;
    };
    sp_records += 1;
    *roots_hist.entry(sp.gc_pair_count()).or_default() += 1;

    // This fixture intentionally contains only statepoints without deopt operands, so the stackmap
    // location count must be `3 header constants + 2 * gc_pairs`.
    assert_eq!(
      sp.header().deopt_count, 0,
      "fixture invariant: expected statepoint deopt_count=0 (record_id=0x{:x})",
      rec.patchpoint_id
    );
    assert_eq!(
      rec.locations.len(),
      LLVM18_STATEPOINT_HEADER_CONSTANTS + 2 * sp.gc_pair_count(),
      "fixture invariant: unexpected statepoint stackmap location count (record_id=0x{:x})",
      rec.patchpoint_id
    );

    for pair in sp.gc_pairs() {
      for loc in [&pair.base, &pair.derived] {
        gc_locs += 1;
        match loc {
          Location::Indirect { size, dwarf_reg, .. } => {
            // `runtime-native` expects pointer-sized spill slots addressed off the caller's SP/FP.
            assert_eq!(
              *size, 8,
              "expected GC root spill slot size=8 (record_id=0x{:x}): {loc:?}",
              rec.patchpoint_id
            );
            assert!(
              matches!(
                *dwarf_reg,
                runtime_native::stackmaps::X86_64_DWARF_REG_RSP | runtime_native::stackmaps::X86_64_DWARF_REG_RBP
              ),
              "expected GC root base register to be RSP/RBP (record_id=0x{:x}): {loc:?}",
              rec.patchpoint_id
            );
          }
          other => {
            panic!(
              "GC root must be an Indirect stack spill slot, got {other:?} (record_id=0x{:x}).\n\
native-js requires stack-slot-only roots at safepoints; ensure LLVM CodeGen options \
`--fixup-allow-gcptr-in-csr=false` / `--fixup-max-csr-statepoints=0` are applied for all codegen paths.",
              rec.patchpoint_id
            );
          }
        }
      }
    }
  }

  assert!(gc_locs > 0, "test bug: expected at least one GC root location");
  if let Some(expected_roots_hist) = expected_roots_hist {
    assert_eq!(
      roots_hist, expected_roots_hist,
      "fixture invariant: unexpected gc root pair histogram"
    );
  }
  assert_eq!(
    sp_records, 12,
    "fixture invariant: expected 12 statepoint records (6 in @inner, 6 in @outer), got {sp_records}"
  );
  assert_eq!(
    sp_records,
    stackmap.records.len(),
    "fixture invariant: expected all stackmap records to be statepoints"
  );
}

#[test]
fn gc_statepoint_stackmaps_do_not_use_register_roots_o3() {
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

  let context = Context::create();
  let ir = include_str!("fixtures/complex_ptr_statepoint.ll");
  assert!(
    ir.contains("gc \"coreclr\""),
    "fixture drift: expected complex_ptr_statepoint.ll to use `gc \"coreclr\"`"
  );
  let buf = MemoryBuffer::create_from_memory_range_copy(ir.as_bytes(), "complex_ptr_statepoint.ll");
  let module = context
    .create_module_from_ir(buf)
    .unwrap_or_else(|err| panic!("failed to parse LLVM IR fixture: {err}"));

  // Emit an object at O3 so register allocation has maximum freedom.
  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let cpu = TargetMachine::get_host_cpu_name().to_string();
  let features = TargetMachine::get_host_cpu_features().to_string();
  let tm = target
    .create_target_machine(
      &triple,
      &cpu,
      &features,
      OptimizationLevel::Aggressive,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine");

  module.set_triple(&triple);
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  if let Err(err) = module.verify() {
    panic!(
      "LLVM IR fixture failed module verification: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let obj = tm
    .write_to_memory_buffer(&module, inkwell::targets::FileType::Object)
    .expect("emit object")
    .as_slice()
    .to_vec();

  let file = object::File::parse(&*obj).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .or_else(|| file.section_by_name("__llvm_stackmaps"))
    .expect("object missing .llvm_stackmaps section");
  let data = section.data().expect("read .llvm_stackmaps section");

  let stackmap = StackMap::parse(data).expect("parse stackmap v3 blob");
  assert_gc_root_locations(
    &stackmap,
    Some(BTreeMap::from([(1, 2), (2, 2), (3, 2), (4, 2), (6, 4)])),
  );
}

#[test]
fn gc_statepoint_stackmaps_do_not_use_register_roots_o3_clang() {
  let Ok(tc) = LlvmToolchain::detect() else {
    eprintln!("skipping: clang not found in PATH");
    return;
  };

  let tmp = tempdir().expect("create tempdir");
  let ll_path = tmp.path().join("complex_ptr_statepoint.ll");
  let obj_path = tmp.path().join("complex_ptr_statepoint.o");

  let ir = include_str!("fixtures/complex_ptr_statepoint.ll");
  fs::write(&ll_path, ir).expect("write LLVM IR fixture");

  // Compile at O3 so register allocation has maximum freedom.
  tc.compile_ll_to_object(&ll_path, &obj_path, OptLevel::O3)
    .expect("clang compile .ll -> .o");

  let obj = fs::read(&obj_path).expect("read object");
  let file = object::File::parse(&*obj).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .or_else(|| file.section_by_name("__llvm_stackmaps"))
    .expect("object missing .llvm_stackmaps section");
  let data = section.data().expect("read .llvm_stackmaps section");

  let stackmap = StackMap::parse(data).expect("parse stackmap v3 blob");
  // Clang may run DCE over unused `gc.relocate` results, which changes the
  // per-record gc-pair histogram. The critical invariant for this test is the
  // *location kind* (stack slot vs register), not the exact root counts.
  assert_gc_root_locations(&stackmap, None);
}
