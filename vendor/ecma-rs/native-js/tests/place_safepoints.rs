use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::{IntPredicate, OptimizationLevel};
use native_js::llvm::{gc, passes};

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
fn place_safepoints_polls_are_rewritten_into_statepoints() {
  let context = Context::create();
  let module = context.create_module("place_safepoints");
  let builder = context.create_builder();

  // Construct a GC-managed function with an unknown-trip-count loop and a GC
  // pointer live across it.
  //
  // The loop body contains no calls, so any statepoints in the output must have
  // come from `place-safepoints` inserting `gc.safepoint_poll` calls.
  let void_ty = context.void_type();
  let i8_ty = context.i8_type();
  let i64_ty = context.i64_type();
  let gc_ptr_ty = gc::gc_ptr_type(&context);

  // define void @test(ptr addrspace(1), i64) gc "coreclr"
  let test_ty = void_ty.fn_type(&[gc_ptr_ty.into(), i64_ty.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let obj = test_fn
    .get_nth_param(0)
    .expect("param 0")
    .into_pointer_value();
  let n = test_fn
    .get_nth_param(1)
    .expect("param 1")
    .into_int_value();

  let entry = context.append_basic_block(test_fn, "entry");
  let loop_header = context.append_basic_block(test_fn, "loop");
  let loop_body = context.append_basic_block(test_fn, "loop_body");
  let exit = context.append_basic_block(test_fn, "exit");

  builder.position_at_end(entry);
  builder
    .build_unconditional_branch(loop_header)
    .expect("br to loop header");

  builder.position_at_end(loop_header);
  let i_phi = builder.build_phi(i64_ty, "i").expect("phi");
  i_phi.add_incoming(&[(&i64_ty.const_zero(), entry)]);

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
  // Keep a GC pointer live across the loop so the inserted polls are "real"
  // safepoints for a GC-managed function.
  builder.build_load(i8_ty, obj, "v").expect("load");
  builder.build_return(None).expect("ret void");

  if let Err(err) = module.verify() {
    panic!(
      "input module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::place_safepoints_and_rewrite_for_gc(&module, &tm)
    .expect("place-safepoints + rewrite-statepoints-for-gc failed");
  // The helper should be safe to call multiple times without re-adding the poll
  // declaration.
  passes::place_safepoints_and_rewrite_for_gc(&module, &tm)
    .expect("place-safepoints + rewrite-statepoints-for-gc (second run) failed");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("declare void @gc.safepoint_poll()"),
    "expected poll function to be predeclared (LLVM 18 place-safepoints workaround):\n{ir}"
  );

  let decl_lines = ir
    .lines()
    .filter(|l| l.starts_with("declare void @gc.safepoint_poll"))
    .count();
  assert_eq!(
    decl_lines, 1,
    "expected exactly one gc.safepoint_poll declaration after running the helper twice:\n{ir}"
  );

  let statepoint_polls = ir
    .lines()
    .filter(|l| l.contains("@llvm.experimental.gc.statepoint") && l.contains("@gc.safepoint_poll"))
    .count();
  assert!(
    statepoint_polls >= 2,
    "expected >=2 statepoints that call gc.safepoint_poll (entry + backedge), got {statepoint_polls}:\n{ir}"
  );

  assert!(
    !ir.contains("call void @gc.safepoint_poll"),
    "expected poll calls to be rewritten (no direct call remains):\n{ir}"
  );
}
