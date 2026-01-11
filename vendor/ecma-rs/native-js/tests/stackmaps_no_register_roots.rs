use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use object::{Object, ObjectSection};
use runtime_native::stackmaps::Location;
use runtime_native::stackmaps::StackMap;
use runtime_native::statepoints::StatepointRecord;

fn assert_no_register_gc_roots(stackmap: &StackMap) {
  let mut gc_locs = 0usize;

  for rec in &stackmap.records {
    let Ok(sp) = StatepointRecord::new(rec) else {
      // Not a statepoint record (could be a patchpoint); ignore.
      continue;
    };

    for pair in sp.gc_pairs() {
      for loc in [&pair.base, &pair.derived] {
        // Constants are not addressable roots; ignore for this regression check.
        if matches!(loc, Location::Constant { .. } | Location::ConstIndex { .. }) {
          continue;
        }

        gc_locs += 1;

        if matches!(loc, Location::Register { .. }) {
          panic!(
            "GC root recorded in a register in .llvm_stackmaps (record_id=0x{:x}): {loc:?}\n\
native-js requires stack-slot-only roots at safepoints; check LLVM CodeGen option \
`--fixup-allow-gcptr-in-csr=false` (or `--fixup-max-csr-statepoints=0`) is applied \
in native-js LLVM init.",
            rec.patchpoint_id
          );
        }
      }
    }
  }

  assert!(gc_locs > 0, "test bug: expected at least one GC root location");
}

#[test]
fn gc_statepoint_stackmaps_do_not_use_register_roots_o3() {
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

  let context = Context::create();
  let ir = include_str!("fixtures/complex_ptr_statepoint.ll");
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
  assert_no_register_gc_roots(&stackmap);
}
