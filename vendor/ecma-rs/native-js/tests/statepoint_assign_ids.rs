#![cfg(feature = "statepoint-directives")]

use inkwell::context::Context;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes, statepoint_directives};
use std::sync::Once;

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
fn assign_statepoint_ids_emits_sequential_ids() {
  let context = Context::create();
  let module = context.create_module("statepoint_assign_ids");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let callee_ty = void_ty.fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);

  let test_ty = void_ty.fn_type(&[], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_statepoint_example_gc(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);
  builder.build_call(callee, &[], "call0").expect("build call0");
  builder.build_call(callee, &[], "call1").expect("build call1");
  builder.build_return(None).expect("build return");

  // Assign deterministic sequential IDs before rewriting.
  statepoint_directives::assign_statepoint_ids(module.as_mut_ptr(), 100).expect("assign statepoint ids");

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");
  let ir = module.print_to_string().to_string();

  let first = ir.find("@llvm.experimental.gc.statepoint.p0(i64 100");
  let second = ir.find("@llvm.experimental.gc.statepoint.p0(i64 101");
  assert!(
    first.is_some() && second.is_some(),
    "expected sequential statepoint IDs 100 and 101 in rewritten IR, got:\n{ir}"
  );
  assert!(
    first.unwrap() < second.unwrap(),
    "expected statepoint-id=100 callsite to appear before statepoint-id=101 in IR, got:\n{ir}"
  );
}

