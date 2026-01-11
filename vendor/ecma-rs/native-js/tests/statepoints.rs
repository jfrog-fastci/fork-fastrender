use inkwell::context::Context;
use inkwell::AddressSpace;

use native_js::gc::statepoints::{LiveGcPtr, StatepointCallee, StatepointIntrinsics};
use native_js::llvm::gc as llvm_gc;

fn set_native_js_gc<'ctx>(func: inkwell::values::FunctionValue<'ctx>) {
  // Required for LLVM's verifier: statepoint intrinsics may only appear in functions which have a
  // GC strategy set.
  llvm_gc::set_default_gc_strategy(&func).expect("GC strategy contains NUL byte");
}

fn set_statepoint_example_gc<'ctx>(func: inkwell::values::FunctionValue<'ctx>) {
  llvm_gc::set_statepoint_example_gc(&func).expect("GC strategy contains NUL byte");
}

#[test]
fn indirect_callee_has_elementtype_attribute() {
  // LLVM 18 (opaque pointers) requires `gc.statepoint`'s callee operand to carry
  // `elementtype(<fn-ty>)` even when the callee is an *indirect* runtime function pointer (`ptr
  // %fp`).
  let context = Context::create();
  let module = context.create_module("m");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let fp_ty = context.ptr_type(AddressSpace::default());
  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1));

  let foo_ty = void_ty.fn_type(&[fp_ty.into(), gc_ptr_ty.into()], false);
  let foo = module.add_function("foo", foo_ty, None);
  set_native_js_gc(foo);

  let fp = foo.get_nth_param(0).unwrap().into_pointer_value();
  fp.set_name("fp");
  let root = foo.get_nth_param(1).unwrap().into_pointer_value();
  root.set_name("root");

  let entry = context.append_basic_block(foo, "entry");
  builder.position_at_end(entry);

  let mut intrinsics = StatepointIntrinsics::new(&module);
  let (_ret, relocated) = intrinsics.emit_statepoint_call(
    &builder,
    StatepointCallee::new(fp, void_ty.fn_type(&[], false)),
    &[],
    &[LiveGcPtr::new(root)],
    None,
  );
  assert_eq!(relocated.len(), 1);

  let _ = builder.build_return(None);
  assert!(module.verify().is_ok(), "module failed LLVM verification");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("ptr elementtype(void ()) %fp"),
    "expected statepoint callee operand to include elementtype(void ()) for indirect call:\n{ir}"
  );
  assert!(
    ir.contains("\"gc-live\"(ptr addrspace(1) %root)"),
    "expected root to appear in gc-live bundle:\n{ir}"
  );
}

#[test]
fn emits_statepoint_result_and_relocates() {
  let context = Context::create();
  let module = context.create_module("m");
  let builder = context.create_builder();

  let i32_ty = context.i32_type();
  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1));

  let callee_ty = i32_ty.fn_type(&[i32_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);

  let caller_ty = i32_ty.fn_type(&[i32_ty.into(), gc_ptr_ty.into(), gc_ptr_ty.into()], false);
  let caller = module.add_function("caller", caller_ty, None);
  set_native_js_gc(caller);

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  let x = caller.get_nth_param(0).unwrap().into_int_value();
  let p1 = caller.get_nth_param(1).unwrap().into_pointer_value();
  let p2 = caller.get_nth_param(2).unwrap().into_pointer_value();

  let mut intrinsics = StatepointIntrinsics::new(&module);
  let (ret, relocated) = intrinsics.emit_statepoint_call(
    &builder,
    StatepointCallee::new(callee.as_global_value().as_pointer_value(), callee_ty),
    &[x.into()],
    &[LiveGcPtr::new(p1), LiveGcPtr::new(p2)],
    Some(i32_ty.into()),
  );

  // Type correctness: relocation must preserve pointer type/address space.
  assert_eq!(relocated.len(), 2);
  assert_eq!(relocated[0].get_type(), p1.get_type());
  assert_eq!(relocated[1].get_type(), p2.get_type());

  let ret = ret.expect("non-void calls must return a value via gc.result");
  let _ = builder.build_return(Some(&ret.into_int_value()));

  assert!(module.verify().is_ok(), "module failed LLVM verification");

  let ir = module.print_to_string().to_string();
  assert!(ir.contains("@llvm.experimental.gc.statepoint"), "{ir}");
  assert!(ir.contains("call token"), "{ir}");
  assert!(ir.contains("@llvm.experimental.gc.result"), "{ir}");

  // One relocate per live GC pointer.
  let relocate_calls = ir
    .lines()
    .filter(|line| line.contains("call") && line.contains("@llvm.experimental.gc.relocate"))
    .count();
  assert_eq!(relocate_calls, 2, "{ir}");

  // Addrspace is preserved (this also implies opaque pointer names, e.g. `.p1`).
  assert!(ir.contains("@llvm.experimental.gc.relocate.p1"), "{ir}");
}

#[test]
fn emits_statepoint_without_gc_result_for_void() {
  let context = Context::create();
  let module = context.create_module("m");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1));

  let callee_ty = context.void_type().fn_type(&[], false);
  let callee = module.add_function("callee_void", callee_ty, None);

  let caller_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let caller = module.add_function("caller_void", caller_ty, None);
  set_native_js_gc(caller);

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  let p = caller.get_nth_param(0).unwrap().into_pointer_value();

  let mut intrinsics = StatepointIntrinsics::new(&module);
  let (ret, relocated) = intrinsics.emit_statepoint_call(
    &builder,
    StatepointCallee::new(callee.as_global_value().as_pointer_value(), callee_ty),
    &[],
    &[LiveGcPtr::new(p)],
    None,
  );

  assert!(ret.is_none(), "void calls must not use gc.result");
  assert_eq!(relocated.len(), 1);
  assert_eq!(relocated[0].get_type(), p.get_type());

  let _ = builder.build_return(None);

  assert!(module.verify().is_ok(), "module failed LLVM verification");

  let ir = module.print_to_string().to_string();
  assert!(ir.contains("@llvm.experimental.gc.statepoint"), "{ir}");
  assert!(ir.contains("call token"), "{ir}");
  assert!(!ir.contains("@llvm.experimental.gc.result"), "{ir}");
  let relocate_calls = ir
    .lines()
    .filter(|line| line.contains("call") && line.contains("@llvm.experimental.gc.relocate"))
    .count();
  assert_eq!(relocate_calls, 1, "{ir}");
}

#[test]
fn gc_pointer_call_args_are_implicitly_in_gc_live() {
  let context = Context::create();
  let module = context.create_module("m");
  let builder = context.create_builder();

  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1));

  let callee_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);

  let caller_ty = context.void_type().fn_type(&[gc_ptr_ty.into()], false);
  let caller = module.add_function("caller", caller_ty, None);
  set_statepoint_example_gc(caller);

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  let p = caller.get_nth_param(0).unwrap().into_pointer_value();
  p.set_name("p");

  let mut intrinsics = StatepointIntrinsics::new(&module);
  let (ret, relocated) = intrinsics.emit_statepoint_call(
    &builder,
    StatepointCallee::new(callee.as_global_value().as_pointer_value(), callee_ty),
    &[p.into()],
    &[],
    None,
  );

  assert!(ret.is_none());
  assert_eq!(relocated.len(), 1);
  assert_eq!(relocated[0].get_type(), p.get_type());

  let _ = builder.build_return(None);
  assert!(module.verify().is_ok(), "module failed LLVM verification");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("\"gc-live\"(ptr addrspace(1) %p)"),
    "{ir}"
  );
}
