use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::types::AsTypeRef as _;
use inkwell::values::AsValueRef as _;
use inkwell::OptimizationLevel;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc;
use object::{Object, ObjectSection};
use runtime_native::stackmaps::{Location, StackMap};
use tempfile::tempdir;

#[test]
fn stackmap_locations_include_gc_pointer_call_args_for_statepoint_emitter() {
  native_js::llvm::init_native_target().expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("statepoint_call_args_stackmap");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let gc_ptr_ty = gc::gc_ptr_type(&context);

  // declare void @callee0()
  let callee0_ty = void_ty.fn_type(&[], false);
  let callee0 = module.add_function("callee0", callee0_ty, None);

  // declare void @callee1(ptr addrspace(1))
  let callee1_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let callee1 = module.add_function("callee1", callee1_ty, None);

  // define void @caller0() gc "coreclr"
  let caller0_ty = void_ty.fn_type(&[], false);
  let caller0 = module.add_function("caller0", caller0_ty, None);
  gc::set_default_gc_strategy(&caller0).expect("GC strategy contains NUL byte");
  let entry0 = context.append_basic_block(caller0, "entry");
  builder.position_at_end(entry0);
  unsafe {
    let mut sp = StatepointEmitter::new(
      (&context).as_ctx_ref(),
      module.as_mut_ptr(),
      gc_ptr_ty.as_type_ref(),
    );
    let _ = sp.emit_statepoint_call(builder.as_mut_ptr(), callee0.as_value_ref(), &[], &[]);
  }
  builder.build_return(None).expect("build return");

  // define void @caller1(ptr addrspace(1)) gc "coreclr"
  let caller1_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let caller1 = module.add_function("caller1", caller1_ty, None);
  gc::set_default_gc_strategy(&caller1).expect("GC strategy contains NUL byte");
  let entry1 = context.append_basic_block(caller1, "entry");
  builder.position_at_end(entry1);

  let p = caller1.get_first_param().unwrap().into_pointer_value();

  unsafe {
    let mut sp = StatepointEmitter::new(
      (&context).as_ctx_ref(),
      module.as_mut_ptr(),
      gc_ptr_ty.as_type_ref(),
    );
    // NOTE: `p` is passed only as a call argument, not as an explicit `gc_live` root.
    let _ = sp.emit_statepoint_call(
      builder.as_mut_ptr(),
      callee1.as_value_ref(),
      &[p.as_value_ref()],
      &[],
    );
  }
  builder.build_return(None).expect("build return");

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
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

  // Ensure the call-arg root survives basic IR cleanup passes. `gc.relocate` is a pure call, so if
  // its result is unused LLVM can DCE it and drop the corresponding root from `.llvm_stackmaps`
  // unless the emitter explicitly anchors it.
  module
    .run_passes("instcombine,dce", &tm, PassBuilderOptions::create())
    .unwrap_or_else(|err| {
      panic!(
        "failed to run instcombine,dce: {err}\n\nIR:\n{}",
        module.print_to_string()
      )
    });

  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoint_call_args_stackmap.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let bytes = std::fs::read(&obj).expect("read object file");
  let file = object::File::parse(&*bytes).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("expected .llvm_stackmaps section");
  let stackmap =
    StackMap::parse(section.data().expect("read .llvm_stackmaps")).expect("parse stackmap v3");

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
