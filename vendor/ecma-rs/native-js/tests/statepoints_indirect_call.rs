use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes, statepoints};
use tempfile::tempdir;

#[test]
fn llvm18_statepoint_rewrite_indirect_call_has_elementtype() {
  // In opaque-pointer mode (LLVM >= 15, and default in LLVM 18), the callee operand of
  // `llvm.experimental.gc.statepoint` must carry an `elementtype(<fn-ty>)`.
  //
  // This is especially important for *indirect calls* through a `ptr`-typed function pointer:
  // the call site's signature must be propagated to the statepoint's callee operand.
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

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
    .build_indirect_call(
      callee_ty,
      fp,
      &[i64_ty.const_int(123, false).into()],
      "call_callee",
    )
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

#[test]
fn llvm18_statepoint_rewrite_indirect_call_nonvoid_has_elementtype_and_gc_result() {
  // Like `llvm18_statepoint_rewrite_indirect_call_has_elementtype`, but the indirect callee returns
  // a scalar so LLVM must also materialize `gc.result.*`.
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

  let context = Context::create();
  let module = context.create_module("statepoints_indirect_call_nonvoid");
  let builder = context.create_builder();

  let i64_ty = context.i64_type();
  let gc_ptr = gc::gc_ptr_type(&context);

  // declare i64 @callee(i64)
  let callee_ty = i64_ty.fn_type(&[i64_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);
  let callee_alt = module.add_function("callee_alt", callee_ty, None);

  // define i64 @test(i64 %x, ptr addrspace(1) %obj) gc "coreclr"
  let test_ty = i64_ty.fn_type(&[i64_ty.into(), gc_ptr.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  let x = test_fn
    .get_nth_param(0)
    .expect("missing %x param")
    .into_int_value();
  x.set_name("x");

  // Build a non-constant function pointer so the call stays indirect.
  let obj = test_fn
    .get_nth_param(1)
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

  // %y = call i64 %fp(i64 %x)  ; indirect call
  let y = builder
    .build_indirect_call(callee_ty, fp, &[x.into()], "call_callee")
    .expect("build indirect call")
    .try_as_basic_value()
    .left()
    .expect("non-void return")
    .into_int_value();
  y.set_name("y");

  // Keep %obj live across the call.
  builder.build_is_null(obj, "isnull").expect("build isnull");
  builder.build_return(Some(&y)).expect("build ret i64");

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

  // Indirect call's callee operand must carry elementtype(i64 (i64)).
  assert!(
    rewritten.contains("ptr elementtype(i64 (i64)) %fp"),
    "expected statepoint callee operand to be `ptr elementtype(i64 (i64)) %fp`, got:\n{rewritten}"
  );

  // Non-void => must materialize gc.result.i64.
  assert!(
    rewritten.contains("@llvm.experimental.gc.result.i64"),
    "expected gc.result.i64 intrinsic for indirect non-void call, got:\n{rewritten}"
  );

  // %obj is live across the call => it must be in the gc-live bundle.
  assert!(
    rewritten.contains("\"gc-live\"(ptr addrspace(1) %obj)"),
    "expected `\"gc-live\"(ptr addrspace(1) %obj)` operand bundle, got:\n{rewritten}"
  );
}

#[test]
fn llvm18_statepoint_rewrite_indirect_call_gcptr_return_has_elementtype_and_gc_result() {
  // Like `llvm18_statepoint_rewrite_indirect_call_has_elementtype`, but the indirect callee returns
  // a GC pointer (`ptr addrspace(1)`) so LLVM must also materialize `gc.result.p1`.
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

  let context = Context::create();
  let module = context.create_module("statepoints_indirect_call_gcptr_return");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);

  // declare ptr addrspace(1) @alloc()
  let callee_ty = gc_ptr.fn_type(&[], false);
  let callee = module.add_function("alloc", callee_ty, None);
  let callee_alt = module.add_function("alloc_alt", callee_ty, None);

  // define ptr addrspace(1) @test(ptr addrspace(1) %obj) gc "coreclr"
  let test_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  // Build a non-constant function pointer so the call stays indirect.
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

  // %p = call ptr addrspace(1) %fp()
  let p = builder
    .build_indirect_call(callee_ty, fp, &[], "call_alloc")
    .expect("build indirect call")
    .try_as_basic_value()
    .left()
    .expect("non-void return")
    .into_pointer_value();
  p.set_name("p");

  // Keep %obj live across the call.
  builder.build_is_null(obj, "isnull").expect("build isnull");
  builder.build_return(Some(&p)).expect("build ret gc ptr");

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

  assert!(
    rewritten.contains("ptr elementtype(ptr addrspace(1) ()) %fp"),
    "expected statepoint callee operand to be `ptr elementtype(ptr addrspace(1) ()) %fp`, got:\n{rewritten}"
  );

  assert!(
    rewritten.contains("@llvm.experimental.gc.result.p1"),
    "expected gc.result.p1 intrinsic for indirect call returning ptr addrspace(1), got:\n{rewritten}"
  );

  assert!(
    rewritten.contains("\"gc-live\"(ptr addrspace(1) %obj)"),
    "expected `\"gc-live\"(ptr addrspace(1) %obj)` operand bundle, got:\n{rewritten}"
  );

  assert!(
    rewritten.contains("call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate for %obj in rewritten IR, got:\n{rewritten}"
  );
}

#[test]
fn llvm18_manual_statepoint_indirect_call_has_elementtype() {
  // Our manual statepoint builder must attach `elementtype(<fn-ty>)` to the callee argument even
  // when the callee is a runtime function pointer (`ptr %fp`).
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

  let context = Context::create();
  let module = context.create_module("manual_statepoints_indirect_call");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let fp_ty = context.ptr_type(inkwell::AddressSpace::default());
  let gc_ptr = gc::gc_ptr_type(&context);

  let foo_ty = void_ty.fn_type(&[fp_ty.into(), gc_ptr.into()], false);
  let foo = module.add_function("foo", foo_ty, None);
  gc::set_default_gc_strategy(&foo).expect("GC strategy contains NUL byte");

  let fp = foo
    .get_nth_param(0)
    .expect("missing %fp")
    .into_pointer_value();
  fp.set_name("fp");

  let root = foo
    .get_nth_param(1)
    .expect("missing %root")
    .into_pointer_value();
  root.set_name("root");

  let entry = context.append_basic_block(foo, "entry");
  builder.position_at_end(entry);

  let callee_sig = void_ty.fn_type(&[], false);
  statepoints::build_statepoint_call_indirect(
    &context,
    &module,
    &builder,
    statepoints::StatepointConfig::default(),
    fp,
    callee_sig,
    &[],
    &[root],
    "sp",
  );
  builder.build_return(None).expect("build return");

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

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed for manual statepoint: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("llvm.experimental.gc.statepoint.p0"),
    "expected gc.statepoint intrinsic in IR, got:\n{ir}"
  );
  assert!(
    ir.contains("ptr elementtype(void ()) %fp"),
    "expected statepoint callee operand to be `ptr elementtype(void ()) %fp`, got:\n{ir}"
  );
  assert!(
    ir.contains("\"gc-live\"(ptr addrspace(1) %root)"),
    "expected `\"gc-live\"(ptr addrspace(1) %root)` operand bundle, got:\n{ir}"
  );

  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("manual_statepoints_indirect_call.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");
}
