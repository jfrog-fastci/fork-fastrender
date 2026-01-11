use std::ffi::CString;

use inkwell::context::Context;
use inkwell::values::AsValueRef;
use inkwell::AddressSpace;
use llvm_sys::core::LLVMSetGC;

use native_js::gc::statepoints::{LiveGcPtr, StatepointCallee, StatepointIntrinsics};

fn set_statepoint_example_gc<'ctx>(func: inkwell::values::FunctionValue<'ctx>) {
  // Required for LLVM's verifier: statepoint intrinsics may only appear in functions which have a
  // GC strategy set.
  let gc_name = CString::new("statepoint-example").unwrap();
  unsafe {
    LLVMSetGC(func.as_value_ref(), gc_name.as_ptr());
  }
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
  set_statepoint_example_gc(caller);

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
  set_statepoint_example_gc(caller);

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
