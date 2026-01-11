use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::values::AsValueRef as _;
use inkwell::AddressSpace;
use llvm_sys::core::{LLVMBuildRetVoid, LLVMGetInsertBlock};
use native_js::gc::roots::GcFrame;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc as llvm_gc;

#[test]
fn gc_frame_safepoint_call_auto_roots_gc_pointer_call_args() {
  let context = Context::create();
  let module = context.create_module("gc_frame_call_args_are_roots");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1u16));

  // declare void @callee(ptr addrspace(1))
  let callee_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);

  // define void @test(ptr addrspace(1)) gc "coreclr"
  let test_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  llvm_gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  let p = test_fn
    .get_nth_param(0)
    .expect("test param 0")
    .into_pointer_value();
  p.set_name("p");

  unsafe {
    let builder_ref = builder.as_mut_ptr();
    let entry_block = LLVMGetInsertBlock(builder_ref);
    let frame = GcFrame::new((&context).as_ctx_ref(), entry_block);
    let mut statepoints =
      StatepointEmitter::new((&context).as_ctx_ref(), module.as_mut_ptr(), frame.gc_ptr_ty());

    // NOTE: `p` is passed *only* as a call argument. `GcFrame` should rely on the statepoint emitter
    // to include it in the gc-live bundle.
    frame.safepoint_call(builder_ref, &mut statepoints, callee.as_value_ref(), &[p.as_value_ref()]);

    LLVMBuildRetVoid(builder_ref);
  }

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("\"gc-live\"(ptr addrspace(1) %p)"),
    "expected call-arg GC pointer to be auto-added to gc-live bundle:\n{ir}"
  );
  assert!(
    ir.contains("@llvm.experimental.gc.relocate.p1"),
    "expected a gc.relocate for the call-arg root:\n{ir}"
  );
}

