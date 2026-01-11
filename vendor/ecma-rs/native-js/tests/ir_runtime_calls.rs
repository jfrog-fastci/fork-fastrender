use inkwell::context::Context;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes};
use native_js::runtime_abi::{RuntimeAbi, RuntimeFn};
use std::sync::Once;

static LLVM_INIT: Once = Once::new();

fn init_llvm() {
  LLVM_INIT.call_once(|| {
    Target::initialize_native(&InitializationConfig::default()).expect("init native target");
  });
}

fn host_target_machine() -> TargetMachine {
  init_llvm();

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

fn build_test_ir() -> String {
  init_llvm();

  let context = Context::create();
  let module = context.create_module("ir_runtime_calls_test");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);

  // define ptr addrspace(1) @test(ptr addrspace(1), ptr addrspace(1)) gc "coreclr"
  let fn_ty = gc_ptr.fn_type(&[gc_ptr.into(), gc_ptr.into()], false);
  let func = module.add_function("test_runtime_calls", fn_ty, None);
  gc::set_default_gc_strategy(&func).expect("set gc strategy");

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);

  let obj = func
    .get_nth_param(0)
    .expect("obj")
    .into_pointer_value();
  let field = func
    .get_nth_param(1)
    .expect("field")
    .into_pointer_value();

  let rt = RuntimeAbi::new(&context, &module);

  // Leaf poll helper: should remain a normal call (never a statepoint).
  let _ = rt
    .emit_runtime_call(&builder, RuntimeFn::GcPoll, &[], "poll")
    .expect("emit gc poll");

  // NoGC call: should *not* be rewritten into a statepoint.
  rt
    .emit_runtime_call(&builder, RuntimeFn::WriteBarrier, &[obj.into(), field.into()], "wb")
    .expect("emit write barrier");

  // NoGC range write barrier: should also remain a normal call.
  let len = context.i64_type().const_int(8, false);
  rt
    .emit_runtime_call(
      &builder,
      RuntimeFn::WriteBarrierRange,
      &[obj.into(), field.into(), len.into()],
      "wbr",
    )
    .expect("emit write barrier range");

  // MayGC call: should become a statepoint after `rewrite-statepoints-for-gc`.
  let size = context.i64_type().const_int(16, false);
  let shape = context.i32_type().const_zero();
  let call = rt
    .emit_runtime_call(&builder, RuntimeFn::Alloc, &[size.into(), shape.into()], "alloc")
    .expect("emit alloc");
  let allocated = call
    .try_as_basic_value()
    .left()
    .expect("rt_alloc returns value")
    .into_pointer_value();

  // KeepAlive is NoGC and must not be rewritten into a statepoint even when it uses a value
  // produced by a MayGC call.
  rt
    .emit_runtime_call(&builder, RuntimeFn::KeepAliveGcRef, &[allocated.into()], "keep_alive")
    .expect("emit keep-alive");

  builder.build_return(Some(&allocated)).expect("ret");

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc");
  if let Err(err) = module.verify() {
    panic!("module verify failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  module.print_to_string().to_string()
}

#[test]
fn alloc_is_statepointed_write_barrier_is_not() {
  let ir = build_test_ir();

  // The statepoint should directly reference the actual GC-triggering callee.
  assert!(ir.contains("@llvm.experimental.gc.statepoint"));
  assert!(
    ir.contains("store ptr @rt_alloc"),
    "missing rt_alloc function pointer materialization:\n{ir}"
  );
  assert!(
    ir.lines()
      .any(|l| l.contains("@llvm.experimental.gc.statepoint") && l.contains("rt.fp.rt_alloc")),
    "statepoint does not call the rt_alloc function pointer:\n{ir}"
  );

  // There should be no direct (non-statepoint) call to the allocator.
  assert!(!ir.contains("call ptr @rt_alloc"));
  assert!(!ir.contains("call ptr addrspace(1) @rt_alloc"));

  // Leaf poll stays a normal call (and must be marked `notail`).
  assert!(
    ir.contains("notail call i1 @rt_gc_poll"),
    "expected a notail call to rt_gc_poll:\n{ir}"
  );
  for line in ir.lines() {
    assert!(
      !(line.contains("@llvm.experimental.gc.statepoint") && line.contains("@rt_gc_poll")),
      "rt_gc_poll should not be statepointed:\n{line}\n\n{ir}"
    );
  }

  // Write barrier stays a normal call and must not be wrapped in a statepoint.
  assert!(ir.contains("notail call void @rt_write_barrier_gc"));
  for line in ir.lines() {
    assert!(
      !(line.contains("@llvm.experimental.gc.statepoint") && line.contains("@rt_write_barrier_gc")),
      "rt_write_barrier_gc should not be statepointed:\n{line}\n\n{ir}"
    );
  }

  assert!(ir.contains("notail call void @rt_write_barrier_range_gc"));
  for line in ir.lines() {
    assert!(
      !(line.contains("@llvm.experimental.gc.statepoint")
        && line.contains("@rt_write_barrier_range_gc")),
      "rt_write_barrier_range_gc should not be statepointed:\n{line}\n\n{ir}"
    );
  }

  assert!(ir.contains("notail call void @rt_keep_alive_gc_ref_gc"));
  for line in ir.lines() {
    assert!(
      !(line.contains("@llvm.experimental.gc.statepoint") && line.contains("@rt_keep_alive_gc_ref_gc")),
      "rt_keep_alive_gc_ref_gc should not be statepointed:\n{line}\n\n{ir}"
    );
  }

  // Regression guard: no wrappers for MayGC runtime entrypoints should exist.
  assert!(!ir.contains("rt_alloc_gc"), "unexpected wrapper function in IR:\n{ir}");
  assert!(
    !ir.contains("rt_gc_safepoint_gc"),
    "unexpected wrapper function in IR:\n{ir}"
  );
}

#[test]
fn runtime_calls_do_not_hide_gc_pointers_via_addrspacecast() {
  let ir = build_test_ir();

  // `native-js` maintains the invariant that GC pointers remain in addrspace(1) form, because
  // `rewrite-statepoints-for-gc` only relocates SSA values of `ptr addrspace(1)`.
  //
  // Converting a GC pointer to addrspace(0) would "hide" it from the pass (and our GC lint rejects
  // such casts), so runtime call emission must avoid `addrspacecast` from addrspace(1).
  assert!(
    !ir.contains("addrspacecast ptr addrspace(1)"),
    "unexpected addrspacecast from gc pointer:\n{ir}"
  );
}
