use inkwell::context::Context;
use inkwell::AddressSpace;
use native_js::runtime_abi::RuntimeAbi;
use native_js::llvm::{gc, lint_module_gc_pointer_discipline, LintRule};

#[test]
fn runtime_abi_wrappers_pass_lint() {
  let context = Context::create();
  let module = context.create_module("gc_lint_wrappers_ok");

  RuntimeAbi::new(&context, &module).ensure_wrappers();

  lint_module_gc_pointer_discipline(&module).unwrap();
}

#[test]
fn rejects_addrspacecast_from_gc_pointer_in_non_wrapper_gc_function() {
  let context = Context::create();
  let module = context.create_module("gc_lint_bad_addrspacecast");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let raw_ptr_ty = context.ptr_type(AddressSpace::default());
  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let func = module.add_function("bad_as_cast", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  let p = func.get_nth_param(0).unwrap().into_pointer_value();
  let _raw = builder
    .build_address_space_cast(p, raw_ptr_ty, "raw")
    .expect("addrspacecast");
  builder.build_return(None).unwrap();

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(
    err.has_rule(LintRule::NonWrapperAddrSpaceCastToOrFromGcPointer),
    "{err}"
  );
}

#[test]
fn rejects_ptrtoint_from_gc_pointer_in_non_wrapper_gc_function() {
  let context = Context::create();
  let module = context.create_module("gc_lint_bad_ptrtoint");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let i64_ty = context.i64_type();
  let fn_ty = i64_ty.fn_type(&[gc_ptr_ty.into()], false);
  let func = module.add_function("bad_ptrtoint", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  let p = func.get_nth_param(0).unwrap().into_pointer_value();
  let i = builder.build_ptr_to_int(p, i64_ty, "i").expect("ptrtoint");
  builder.build_return(Some(&i)).unwrap();

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(err.has_rule(LintRule::PtrToIntFromGcPointer), "{err}");
}

#[test]
fn rejects_inttoptr_to_gc_pointer_in_non_wrapper_gc_function() {
  let context = Context::create();
  let module = context.create_module("gc_lint_bad_inttoptr");
  let builder = context.create_builder();

  let i64_ty = context.i64_type();
  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let fn_ty = gc_ptr_ty.fn_type(&[i64_ty.into()], false);
  let func = module.add_function("bad_inttoptr", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  let i = func.get_nth_param(0).unwrap().into_int_value();
  let p = builder
    .build_int_to_ptr(i, gc_ptr_ty, "p")
    .expect("inttoptr");
  builder.build_return(Some(&p)).unwrap();

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(err.has_rule(LintRule::IntToPtrToGcPointer), "{err}");
}

#[test]
fn rejects_store_of_gc_pointer_into_non_pointer_slot() {
  let context = Context::create();
  let module = context.create_module("gc_lint_bad_store");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let func = module.add_function("bad_store", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  let p = func.get_nth_param(0).unwrap().into_pointer_value();

  let slot = builder.build_alloca(context.i64_type(), "slot").unwrap();
  builder.build_store(slot, p).unwrap();
  builder.build_return(None).unwrap();

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(err.has_rule(LintRule::StoreGcPointerToNonPointerSlot), "{err}");
}

#[test]
fn rejects_addrspacecast_from_gc_pointer_in_wrapper_gc_function() {
  let context = Context::create();
  let module = context.create_module("gc_lint_bad_wrapper_addrspacecast");
  let builder = context.create_builder();

  // Name matches runtime ABI wrapper convention.
  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let raw_ptr_ty = context.ptr_type(AddressSpace::default());
  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let func = module.add_function("rt_bad_gc", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  let p = func.get_nth_param(0).unwrap().into_pointer_value();
  let _raw = builder
    .build_address_space_cast(p, raw_ptr_ty, "raw")
    .expect("addrspacecast");
  builder.build_return(None).unwrap();

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(
    err.has_rule(LintRule::WrapperAddrSpaceCastAs1ToAs0InvalidUse),
    "{err}"
  );
}
