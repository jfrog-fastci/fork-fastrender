use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::gc::statepoints::{StatepointCallee, StatepointIntrinsics};
use native_js::llvm::gc;
use object::{Object, ObjectSection};
use runtime_native::stackmaps::{Location, StackMap};
use tempfile::tempdir;

#[test]
fn stackmap_locations_include_gc_pointer_call_args() {
  Target::initialize_native(&InitializationConfig::default()).expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("statepoints_call_args_stackmap");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let gc_ptr_ty = gc::gc_ptr_type(&context);

  // declare void @callee0()
  let callee0_ty = void_ty.fn_type(&[], false);
  let callee0 = module.add_function("callee0", callee0_ty, None);

  // declare void @callee1(ptr addrspace(1))
  let callee1_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let callee1 = module.add_function("callee1", callee1_ty, None);

  let mut intrinsics = StatepointIntrinsics::new(&module);

  // define void @caller0() gc "coreclr"
  let caller0_ty = void_ty.fn_type(&[], false);
  let caller0 = module.add_function("caller0", caller0_ty, None);
  gc::set_default_gc_strategy(&caller0).expect("GC strategy contains NUL byte");
  let entry0 = context.append_basic_block(caller0, "entry");
  builder.position_at_end(entry0);
  intrinsics.emit_statepoint_call(
    &builder,
    StatepointCallee::new(callee0.as_global_value().as_pointer_value(), callee0_ty),
    &[],
    &[],
    None,
  );
  builder.build_return(None).expect("build return");

  // define ptr addrspace(1) @caller1(ptr addrspace(1) %p) gc "coreclr"
  let caller1_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into()], false);
  let caller1 = module.add_function("caller1", caller1_ty, None);
  gc::set_default_gc_strategy(&caller1).expect("GC strategy contains NUL byte");
  let entry1 = context.append_basic_block(caller1, "entry");
  builder.position_at_end(entry1);

  let p = caller1.get_first_param().unwrap().into_pointer_value();
  p.set_name("p");

  let (_ret, relocated) = intrinsics.emit_statepoint_call(
    &builder,
    StatepointCallee::new(callee1.as_global_value().as_pointer_value(), callee1_ty),
    &[p.into()],
    &[],
    None,
  );
  assert_eq!(relocated.len(), 1);
  builder
    .build_return(Some(&relocated[0]))
    .expect("build return");

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let tm = target
    .create_target_machine(
      &triple,
      "generic",
      "",
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine");
  module.set_triple(&triple);
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints_call_args_stackmap.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let bytes = std::fs::read(&obj).expect("read object file");
  let file = object::File::parse(&*bytes).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("expected .llvm_stackmaps section");
  let stackmap = StackMap::parse(section.data().expect("read .llvm_stackmaps")).expect("parse stackmap v3");

  assert!(
    stackmap.records.len() >= 2,
    "expected at least 2 stackmap records, got {}\n\nIR:\n{}",
    stackmap.records.len(),
    module.print_to_string()
  );

  let is_const_loc =
    |loc: &Location| matches!(loc, Location::Constant { .. } | Location::ConstIndex { .. });
  let min_locations = stackmap
    .records
    .iter()
    .map(|r| r.locations.len())
    .min()
    .unwrap();
  let max_locations = stackmap
    .records
    .iter()
    .map(|r| r.locations.len())
    .max()
    .unwrap();

  assert!(
    max_locations > min_locations,
    "expected a record with more locations when a GC pointer call arg is present; min={min_locations} max={max_locations}\n\nstackmap={stackmap:?}\n\nIR:\n{}",
    module.print_to_string()
  );

  let max_record = stackmap
    .records
    .iter()
    .find(|r| r.locations.len() == max_locations)
    .unwrap();
  assert!(
    max_record.locations.iter().any(|loc| !is_const_loc(loc)),
    "expected the larger record to include at least one non-constant location\n\nrecord={max_record:?}\n\nIR:\n{}",
    module.print_to_string()
  );

  let min_record = stackmap
    .records
    .iter()
    .find(|r| r.locations.len() == min_locations)
    .unwrap();
  assert!(
    min_record.locations.iter().all(is_const_loc),
    "expected the smaller record to contain only constant locations\n\nrecord={min_record:?}\n\nIR:\n{}",
    module.print_to_string()
  );
}
