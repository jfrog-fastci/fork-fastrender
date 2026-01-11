use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::AddressSpace;
use native_js::llvm::{gc, lint_module_gc_pointer_discipline, LintRule};
use native_js::runtime_abi::{RuntimeAbi, RuntimeFn};

#[test]
fn runtime_abi_emission_passes_lint() {
  let context = Context::create();
  let module = context.create_module("gc_lint_runtime_calls_ok");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into(), gc_ptr_ty.into()], false);
  let func = module.add_function("test_runtime_calls", fn_ty, None);
  gc::set_default_gc_strategy(&func).expect("set gc strategy");

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);

  let obj = func.get_nth_param(0).unwrap().into_pointer_value();
  let field = func.get_nth_param(1).unwrap().into_pointer_value();

  let rt = RuntimeAbi::new(&context, &module);
  let _ = rt
    .emit_runtime_call(
      &builder,
      RuntimeFn::WriteBarrier,
      &[obj.into(), field.into()],
      "wb",
    )
    .expect("emit write barrier");
  let size = context.i64_type().const_int(16, false);
  let shape = context.i32_type().const_zero();
  let _ = rt
    .emit_runtime_call(&builder, RuntimeFn::Alloc, &[size.into(), shape.into()], "alloc")
    .expect("emit alloc");
  builder.build_return(None).unwrap();

  lint_module_gc_pointer_discipline(&module).unwrap();
}

#[test]
fn good_gc_managed_function_passes() {
  let context = Context::create();
  let module = context.create_module("gc_lint_good");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let func = module.add_function("good", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  builder.build_return(None).unwrap();

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
fn rejects_store_of_gc_pointer_into_addrspace0_pointer_slot() {
  let context = Context::create();
  let module = context.create_module("gc_lint_bad_store_ptr");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let raw_ptr_ty = context.ptr_type(AddressSpace::default());
  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let func = module.add_function("bad_store_ptr", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  let p = func.get_nth_param(0).unwrap().into_pointer_value();

  let slot = builder.build_alloca(raw_ptr_ty, "slot").unwrap();
  builder.build_store(slot, p).unwrap();
  builder.build_return(None).unwrap();

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(
    err.has_rule(LintRule::StoreGcPointerToNonGcPointerSlot),
    "{err}"
  );
}

#[test]
fn rejects_call_with_raw_pointer_derived_from_gc_pointer() {
  let context = Context::create();
  let module = context.create_module("gc_lint_bad_call");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let raw_ptr_ty = context.ptr_type(AddressSpace::default());

  let sink_ty = context.void_type().fn_type(&[raw_ptr_ty.into()], false);
  let sink = module.add_function("sink", sink_ty, None);

  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let func = module.add_function("bad_call", fn_ty, None);
  func.set_gc(gc::GC_STRATEGY);

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);
  let p = func.get_nth_param(0).unwrap().into_pointer_value();

  // Launder the GC pointer through a local stack slot and load it as `ptr`.
  //
  // The alloca is typed as a GC pointer slot (`ptr addrspace(1)`), but we load it as `ptr` to
  // simulate accidentally treating a rooted GC pointer as a raw pointer alias.
  let slot = builder.build_alloca(gc_ptr_ty, "slot").unwrap();
  builder.build_store(slot, p).unwrap();
  let raw = builder
    .build_load(raw_ptr_ty, slot, "raw")
    .unwrap()
    .into_pointer_value();

  builder.build_call(sink, &[raw.into()], "").unwrap();
  builder.build_return(None).unwrap();

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(
    err.has_rule(LintRule::CallAddrSpace0PointerDerivedFromGcPointer),
    "{err}"
  );
  assert!(
    err.has_rule(LintRule::AddrSpace0PointerDerivedFromGcPointer),
    "{err}"
  );
}

#[test]
fn rejects_callbr_with_raw_pointer_derived_from_gc_pointer() {
  let context = Context::create();

  let llvm_ir = r#"
define void @test(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %slot = alloca ptr addrspace(1)
  store ptr addrspace(1) %obj, ptr %slot
  %raw = load ptr, ptr %slot
  callbr void asm sideeffect "", "r,!i"(ptr %raw) to label %cont [label %l1]
cont:
  ret void
l1:
  br label %cont
}
"#;

  let buffer = MemoryBuffer::create_from_memory_range_copy(llvm_ir.as_bytes(), "test.ll");
  let module = context
    .create_module_from_ir(buffer)
    .unwrap_or_else(|err| panic!("failed to parse LLVM IR: {err}\n\nIR:\n{llvm_ir}"));

  let err = lint_module_gc_pointer_discipline(&module).unwrap_err();
  assert!(
    err.has_rule(LintRule::CallAddrSpace0PointerDerivedFromGcPointer),
    "{err}"
  );
  assert!(
    err.has_rule(LintRule::AddrSpace0PointerDerivedFromGcPointer),
    "{err}"
  );
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

#[test]
fn non_gc_wrapper_may_addrspacecast() {
  let context = Context::create();
  let module = context.create_module("gc_lint_non_gc_wrapper_ok");
  let builder = context.create_builder();

  // Not GC-managed: should be ignored by the lint.
  let gc_ptr_ty = context.ptr_type(gc::gc_address_space());
  let raw_ptr_ty = context.ptr_type(AddressSpace::default());
  let fn_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let wrapper = module.add_function("rt_alloc_gc", fn_ty, None);

  let entry = context.append_basic_block(wrapper, "entry");
  builder.position_at_end(entry);
  let p = wrapper.get_nth_param(0).unwrap().into_pointer_value();
  let _raw = builder
    .build_address_space_cast(p, raw_ptr_ty, "raw")
    .expect("addrspacecast");
  builder.build_return(None).unwrap();

  lint_module_gc_pointer_discipline(&module).unwrap();
}
