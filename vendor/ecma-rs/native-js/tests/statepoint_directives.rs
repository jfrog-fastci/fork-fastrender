use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::values::AsValueRef;
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes, statepoint_directives};
use object::{Object, ObjectSection};
use runtime_native::stackmap::StackMap;
use std::sync::Once;
use tempfile::tempdir;

static LLVM_INIT: Once = Once::new();

fn host_target_machine() -> TargetMachine {
  LLVM_INIT.call_once(|| {
    Target::initialize_native(&InitializationConfig::default()).expect("failed to init native target");
  });

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
fn rewrite_statepoints_honors_callsite_directives() {
  let context = Context::create();
  let module = context.create_module("statepoint_directives");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let bar_ty = void_ty.fn_type(&[], false);
  let bar = module.add_function("bar", bar_ty, None);

  let foo_ty = void_ty.fn_type(&[], false);
  let foo = module.add_function("foo", foo_ty, None);
  // `rewrite-statepoints-for-gc` only rewrites callsites in functions marked with a GC strategy.
  gc::set_default_gc_strategy(&foo).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(foo, "entry");
  builder.position_at_end(entry);

  let call = builder.build_call(bar, &[], "call_bar").expect("build call");
  statepoint_directives::set_callsite_statepoint_id(call.as_value_ref(), 42);
  statepoint_directives::set_callsite_statepoint_num_patch_bytes(call.as_value_ref(), 16);
  builder.build_return(None).expect("build return");

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint.p0(i64 42, i32 16"),
    "expected statepoint id/patch-bytes in rewritten IR, got:\n{ir}"
  );

  // Stronger check: the statepoint ID is the StackMap patchpoint ID (encoded as a u64 in
  // `.llvm_stackmaps`).
  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let data = std::fs::read(&obj).expect("read emitted object");
  let file = object::File::parse(&*data).expect("parse emitted object");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let stackmaps = StackMap::parse(section.data().expect("read .llvm_stackmaps section bytes"))
    .expect("parse .llvm_stackmaps");

  assert_eq!(
    stackmaps.records.len(),
    1,
    "expected exactly 1 stackmap record, got {}\nIR:\n{ir}",
    stackmaps.records.len()
  );
  assert_eq!(
    stackmaps.records[0].patchpoint_id, 42,
    "expected stackmap patchpoint_id to match statepoint-id=42\nIR:\n{ir}"
  );
}

#[test]
fn rewrite_statepoints_uses_default_statepoint_id_when_unset() {
  let context = Context::create();
  let module = context.create_module("statepoint_default_id");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let bar_ty = void_ty.fn_type(&[], false);
  let bar = module.add_function("bar", bar_ty, None);

  let foo_ty = void_ty.fn_type(&[], false);
  let foo = module.add_function("foo", foo_ty, None);
  gc::set_default_gc_strategy(&foo).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(foo, "entry");
  builder.position_at_end(entry);
  builder.build_call(bar, &[], "call_bar").expect("build call");
  builder.build_return(None).expect("build return");

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint.p0(i64 2882400000, i32 0"),
    "expected default statepoint id/patch-bytes in rewritten IR, got:\n{ir}"
  );

  // The default statepoint ID is also used as the StackMap record patchpoint ID.
  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let data = std::fs::read(&obj).expect("read emitted object");
  let file = object::File::parse(&*data).expect("parse emitted object");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let stackmaps = StackMap::parse(section.data().expect("read .llvm_stackmaps section bytes"))
    .expect("parse .llvm_stackmaps");

  assert_eq!(
    stackmaps.records.len(),
    1,
    "expected exactly 1 stackmap record, got {}\nIR:\n{ir}",
    stackmaps.records.len()
  );
  assert_eq!(
    stackmaps.records[0].patchpoint_id, 2882400000,
    "expected default StackMap patchpoint_id to be 2882400000 (0xABCDEF00)\nIR:\n{ir}"
  );
}
