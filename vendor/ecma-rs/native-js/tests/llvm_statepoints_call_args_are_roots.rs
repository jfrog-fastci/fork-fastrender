use inkwell::context::Context;
use native_js::llvm::{gc, statepoints};

#[test]
fn manual_statepoint_builder_includes_gc_pointer_call_args_in_gc_live() {
  let context = Context::create();
  let module = context.create_module("llvm_statepoints_call_args_are_roots");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let gc_ptr_ty = gc::gc_ptr_type(&context);

  let callee_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);

  let caller_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let caller = module.add_function("caller", caller_ty, None);
  gc::set_default_gc_strategy(&caller).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  let p = caller.get_first_param().unwrap().into_pointer_value();
  p.set_name("p");

  statepoints::build_statepoint_call_direct(
    &context,
    &module,
    &builder,
    statepoints::StatepointConfig::default(),
    callee,
    &[p.into()],
    &[],
    "sp",
  );
  builder.build_return(None).expect("build return");

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("\"gc-live\"(ptr addrspace(1) %p)"),
    "expected call-arg GC pointer to be auto-added to gc-live bundle:\n{ir}"
  );
}
