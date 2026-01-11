use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes};
use object::{Object, ObjectSection};
use runtime_native::stackmaps::StackMap;
use runtime_native::statepoints::StatepointRecord;
use tempfile::tempdir;

fn host_target_machine() -> TargetMachine {
  native_js::llvm::init_native_target().expect("failed to init native target");

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let cpu = TargetMachine::get_host_cpu_name().to_string();
  let features = TargetMachine::get_host_cpu_features().to_string();
  target
    .create_target_machine(
      &triple,
      &cpu,
      &features,
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine")
}

#[test]
fn stackmap_gc_pair_count_matches_gc_pointer_liveness() {
  let context = Context::create();
  let module = context.create_module("gc_liveness_stackmap");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let i8_ty = context.i8_type();
  let gc_ptr_ty = gc::gc_ptr_type(&context);

  let may_gc_ty = void_ty.fn_type(&[], false);
  let may_gc = module.add_function("may_gc", may_gc_ty, None);

  // define void @test(ptr addrspace(1) %p1, ptr addrspace(1) %p2) gc "coreclr"
  let test_ty = void_ty.fn_type(&[gc_ptr_ty.into(), gc_ptr_ty.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  let p1 = test_fn
    .get_nth_param(0)
    .expect("param 0")
    .into_pointer_value();
  p1.set_name("p1");
  let p2 = test_fn
    .get_nth_param(1)
    .expect("param 1")
    .into_pointer_value();
  p2.set_name("p2");

  // Use `p2` only before the safepoint so it should *not* be live across it.
  builder
    .build_load(i8_ty, p2, "p2.pre")
    .expect("build p2 load");

  builder
    .build_call(may_gc, &[], "call_may_gc")
    .expect("build call");

  // Use `p1` after the safepoint so it *is* live across it.
  builder
    .build_load(i8_ty, p1, "p1.post")
    .expect("build p1 load");

  builder.build_return(None).expect("build return");

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed after rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let ir = module.print_to_string().to_string();

  let tmp = tempdir().expect("failed to create tempdir");
  let obj_path = tmp.path().join("gc_liveness_stackmap.o");
  tm.write_to_file(&module, FileType::Object, &obj_path)
    .expect("emit object file");
  let bytes = std::fs::read(&obj_path).expect("read object file");
  let file = object::File::parse(&*bytes).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let stackmap = StackMap::parse(section.data().expect("read .llvm_stackmaps"))
    .expect("parse stackmap v3");

  assert_eq!(
    stackmap.records.len(),
    1,
    "expected exactly one stackmap record for the rewritten `may_gc` call, got {}\n\nstackmap={stackmap:?}\n\nIR:\n{ir}",
    stackmap.records.len()
  );

  let sp = StatepointRecord::new(&stackmap.records[0]).expect("decode statepoint record");
  assert_eq!(
    sp.gc_pair_count(),
    1,
    "expected only `%p1` to be live across the safepoint (so one gc.relocate/stackmap pair), got {}.\n\nIR:\n{ir}",
    sp.gc_pair_count()
  );
}

