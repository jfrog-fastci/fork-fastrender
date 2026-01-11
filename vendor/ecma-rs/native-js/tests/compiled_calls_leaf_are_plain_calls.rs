use inkwell::attributes::AttributeLoc;
use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::values::AsValueRef as _;
use inkwell::AddressSpace;
use llvm_sys::core::{LLVMBuildRet, LLVMGetInsertBlock};
use native_js::gc::roots::GcFrame;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc as llvm_gc;

#[test]
fn compiled_calls_to_gc_leaf_functions_are_plain_calls() {
  let context = Context::create();
  let module = context.create_module("compiled_calls_leaf_plain_calls");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1u16));

  // Leaf callee: returns its argument and is explicitly annotated as a GC leaf.
  let callee_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);
  llvm_gc::set_default_gc_strategy(&callee).expect("GC strategy contains NUL byte");
  let leaf_attr = context.create_string_attribute("gc-leaf-function", "");
  callee.add_attribute(AttributeLoc::Function, leaf_attr);

  let entry = context.append_basic_block(callee, "entry");
  builder.position_at_end(entry);
  let arg0 = callee
    .get_nth_param(0)
    .expect("callee param 0")
    .into_pointer_value();
  builder.build_return(Some(&arg0)).expect("build callee return");

  // Caller: roots its argument but (because the callee is annotated as a leaf)
  // should still emit a *plain* call rather than a statepoint callsite.
  let caller_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into()], false);
  let caller = module.add_function("caller", caller_ty, None);
  llvm_gc::set_default_gc_strategy(&caller).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  unsafe {
    let builder_ref = builder.as_mut_ptr();
    let entry_block = LLVMGetInsertBlock(builder_ref);
    let frame = GcFrame::new((&context).as_ctx_ref(), entry_block);
    let mut statepoints =
      StatepointEmitter::new((&context).as_ctx_ref(), module.as_mut_ptr(), frame.gc_ptr_ty());

    let arg = caller
      .get_nth_param(0)
      .expect("caller param 0")
      .into_pointer_value();
    arg.set_name("arg");

    let _ = frame.root_base(builder_ref, arg.as_value_ref());

    let res = frame
      .compiled_call(
        builder_ref,
        &mut statepoints,
        callee.as_value_ref(),
        &[arg.as_value_ref()],
        None,
      )
      .expect("callee returns ptr addrspace(1)");

    LLVMBuildRet(builder_ref, res);
  }

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let ir = module.print_to_string().to_string();

  assert!(
    ir.contains("call ptr addrspace(1) @callee"),
    "expected a plain direct call to @callee, got:\n{ir}"
  );
  assert!(
    !ir.contains("call token @llvm.experimental.gc.statepoint.p0"),
    "expected no statepoint callsites (leaf callee call must be plain), got:\n{ir}"
  );
}

