use inkwell::context::Context;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes};

#[test]
fn llvm18_statepoint_rewrite_indirect_call_has_elementtype() {
  // In opaque-pointer mode (LLVM >= 15, and default in LLVM 18), the callee operand of
  // `llvm.experimental.gc.statepoint` must carry an `elementtype(<fn-ty>)`.
  //
  // This is especially important for *indirect calls* through a `ptr`-typed function pointer:
  // the call site's signature must be propagated to the statepoint's callee operand.
  Target::initialize_native(&InitializationConfig::default()).expect("failed to initialize native LLVM target");

  let context = Context::create();
  let module = context.create_module("statepoints_indirect_call");
  let builder = context.create_builder();

  let i64_ty = context.i64_type();
  let void_ty = context.void_type();

  // declare void @callee(i64)
  let callee_ty = void_ty.fn_type(&[i64_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);
  let callee_alt = module.add_function("callee_alt", callee_ty, None);

  // define void @test(ptr addrspace(1) %obj) gc "coreclr"
  let gc_ptr = gc::gc_ptr_type(&context);
  let test_ty = void_ty.fn_type(&[gc_ptr.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  // Build a non-constant function pointer so this call stays indirect even if LLVM performs
  // simple canonicalization on the IR.
  let obj = test_fn
    .get_first_param()
    .expect("missing %obj param")
    .into_pointer_value();
  obj.set_name("obj");

  let isnull_pre = builder
    .build_is_null(obj, "isnull_pre")
    .expect("build isnull_pre");
  let fp = builder
    .build_select(
      isnull_pre,
      callee.as_global_value().as_pointer_value(),
      callee_alt.as_global_value().as_pointer_value(),
      "fp",
    )
    .expect("select function pointer")
    .into_pointer_value();

  // call void %fp(i64 123)  ; indirect call
  builder
    .build_indirect_call(callee_ty, fp, &[i64_ty.const_int(123, false).into()], "call_callee")
    .expect("build indirect call");

  // Use %obj after the call so it is live across the safepoint.
  builder.build_is_null(obj, "isnull").expect("build isnull");
  builder.build_return(None).expect("build ret void");

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

  passes::rewrite_statepoints_for_gc(&module, &tm).unwrap_or_else(|err| {
    panic!(
      "rewrite-statepoints-for-gc failed: {err}\n\nAfter:\n{}",
      module.print_to_string()
    )
  });

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed after rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let rewritten = module.print_to_string().to_string();

  // Statepoint inserted.
  assert!(
    rewritten.contains("call token") && rewritten.contains("llvm.experimental.gc.statepoint.p0"),
    "expected gc.statepoint intrinsic in rewritten IR, got:\n{rewritten}"
  );

  // Indirect call's callee operand must carry elementtype(void (i64)).
  assert!(
    rewritten.contains("ptr elementtype(void (i64)) %fp"),
    "expected statepoint callee operand to be `ptr elementtype(void (i64)) %fp`, got:\n{rewritten}"
  );

  // %obj is live across the call => it must be in the gc-live bundle.
  assert!(
    rewritten.contains("\"gc-live\"(ptr addrspace(1) %obj)"),
    "expected `\"gc-live\"(ptr addrspace(1) %obj)` operand bundle, got:\n{rewritten}"
  );

  // ...and thus a relocate for %obj must exist.
  assert!(
    rewritten.contains("call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate for %obj in rewritten IR, got:\n{rewritten}"
  );
}
