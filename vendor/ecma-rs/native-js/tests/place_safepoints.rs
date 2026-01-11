use inkwell::context::Context;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::{IntPredicate, OptimizationLevel};
use native_js::llvm::{gc, passes};
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
fn place_safepoints_polls_are_rewritten_into_statepoints() {
  let context = Context::create();
  let module = context.create_module("place_safepoints");
  let builder = context.create_builder();

  // Construct a GC-managed function with an unknown-trip-count loop.
  //
  // The loop body contains no calls, so any statepoint in the output must have
  // come from `place-safepoints` inserting `gc.safepoint_poll` calls.
  let void_ty = context.void_type();
  let i64_ty = context.i64_type();
  let foo_ty = void_ty.fn_type(&[i64_ty.into()], false);
  let foo = module.add_function("foo", foo_ty, None);
  gc::set_default_gc_strategy(&foo).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(foo, "entry");
  let loop_header = context.append_basic_block(foo, "loop");
  let loop_body = context.append_basic_block(foo, "loop_body");
  let exit = context.append_basic_block(foo, "exit");

  builder.position_at_end(entry);
  builder
    .build_unconditional_branch(loop_header)
    .expect("br to loop header");

  builder.position_at_end(loop_header);
  let i_phi = builder.build_phi(i64_ty, "i").expect("phi");
  i_phi.add_incoming(&[(&i64_ty.const_zero(), entry)]);

  let n = foo
    .get_nth_param(0)
    .expect("param 0")
    .into_int_value();
  let i = i_phi.as_basic_value().into_int_value();
  let cond = builder
    .build_int_compare(IntPredicate::ULT, i, n, "cond")
    .expect("icmp");
  builder
    .build_conditional_branch(cond, loop_body, exit)
    .expect("condbr");

  builder.position_at_end(loop_body);
  let i_next = builder
    .build_int_add(i, i64_ty.const_int(1, false), "i.next")
    .expect("add");
  builder
    .build_unconditional_branch(loop_header)
    .expect("backedge");
  i_phi.add_incoming(&[(&i_next, loop_body)]);

  builder.position_at_end(exit);
  builder.build_return(None).expect("ret void");

  if let Err(err) = module.verify() {
    panic!("input module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::place_safepoints_and_rewrite_statepoints_for_gc(&module, &tm)
    .expect("place-safepoints + rewrite-statepoints-for-gc failed");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("declare void @gc.safepoint_poll()"),
    "expected poll function to be predeclared (LLVM 18 place-safepoints workaround):\n{ir}"
  );
  assert!(
    ir.contains("llvm.experimental.gc.statepoint"),
    "expected poll call to be rewritten into a statepoint:\n{ir}"
  );
  assert!(
    !ir.contains("call void @gc.safepoint_poll"),
    "expected poll calls to be rewritten (no direct call remains):\n{ir}"
  );
}

