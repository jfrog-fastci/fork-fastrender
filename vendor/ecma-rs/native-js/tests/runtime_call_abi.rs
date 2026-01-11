use inkwell::context::Context;
use native_js::llvm::gc;
use native_js::runtime_abi::{emit_runtime_call, ArgRootingPolicy, RuntimeCallError, RuntimeFn, RuntimeFnSpec};

#[test]
fn runtime_call_registry_has_gc_safety_metadata() {
  // MayGC functions with no GC pointer args.
  for f in [
    RuntimeFn::Alloc,
    RuntimeFn::AllocPinned,
    RuntimeFn::GcSafepoint,
    RuntimeFn::GcCollect,
  ] {
    let spec = f.spec();
    assert!(
      spec.may_gc,
      "expected {f:?} to be may_gc=true, got {spec:?}"
    );
    assert_eq!(
      spec.gc_ptr_args, 0,
      "expected {f:?} to have 0 GC pointer args, got {spec:?}"
    );
  }

  // NoGC functions are allowed to take GC pointer args.
  let wb = RuntimeFn::WriteBarrier.spec();
  assert!(
    !wb.may_gc,
    "expected rt_write_barrier to be may_gc=false, got {wb:?}"
  );
  assert!(
    wb.gc_ptr_args > 0,
    "expected rt_write_barrier to take GC pointer args, got {wb:?}"
  );

  let ka = RuntimeFn::KeepAliveGcRef.spec();
  assert!(
    !ka.may_gc,
    "expected rt_keep_alive_gc_ref to be may_gc=false, got {ka:?}"
  );
  assert!(
    ka.gc_ptr_args > 0,
    "expected rt_keep_alive_gc_ref to take GC pointer args, got {ka:?}"
  );
}

#[test]
fn rejects_may_gc_runtime_fn_with_gc_pointer_args() {
  let context = Context::create();
  let module = context.create_module("runtime_call_abi_test");
  let builder = context.create_builder();

  // Create a dummy caller so the builder has an insertion point.
  let caller = module.add_function("caller", context.void_type().fn_type(&[], false), None);
  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  // Mock runtime function signature: `void (ptr addrspace(1))`.
  let gc_ptr = gc::gc_ptr_type(&context);
  let callee = module.add_function(
    "rt_bad_may_gc_with_ptr",
    context.void_type().fn_type(&[gc_ptr.into()], false),
    None,
  );

  let spec = RuntimeFnSpec {
    name: "rt_bad_may_gc_with_ptr",
    may_gc: true,
    gc_ptr_args: 1,
    arg_rooting: ArgRootingPolicy::NoGcPointersAllowedIfMayGc,
  };

  let err = emit_runtime_call(
    &builder,
    callee,
    spec,
    &[gc_ptr.const_null().into()],
    "call_bad",
  )
  .unwrap_err();

  match err {
    RuntimeCallError::MayGcWithGcPointerArgs { .. } => {}
    other => panic!("expected MayGcWithGcPointerArgs error, got {other:?}"),
  }
}

#[test]
fn allows_may_gc_runtime_fn_with_gc_pointer_args_if_runtime_roots() {
  let context = Context::create();
  let module = context.create_module("runtime_call_abi_test_roots");
  let builder = context.create_builder();

  // Create a dummy caller so the builder has an insertion point.
  let caller = module.add_function("caller", context.void_type().fn_type(&[], false), None);
  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  // Mock runtime function signature: `void (ptr addrspace(1))`.
  let gc_ptr = gc::gc_ptr_type(&context);
  let callee = module.add_function(
    "rt_may_gc_with_ptr_but_roots",
    context.void_type().fn_type(&[gc_ptr.into()], false),
    None,
  );

  let spec = RuntimeFnSpec {
    name: "rt_may_gc_with_ptr_but_roots",
    may_gc: true,
    gc_ptr_args: 1,
    arg_rooting: ArgRootingPolicy::RuntimeRootsPointers,
  };

  emit_runtime_call(
    &builder,
    callee,
    spec,
    &[gc_ptr.const_null().into()],
    "call_ok",
  )
  .expect("call should be allowed when runtime_roots_args=true");
}
