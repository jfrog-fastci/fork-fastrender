use std::ffi::{CStr, CString};

use inkwell::context::Context;
use inkwell::types::BasicType;
use inkwell::values::AsValueRef;
use inkwell::AddressSpace;
use llvm_sys::analysis::{LLVMVerifierFailureAction, LLVMVerifyModule};
use llvm_sys::core::{LLVMDisposeMessage, LLVMPrintModuleToString, LLVMSetDataLayout, LLVMSetGC, LLVMSetTarget};
use llvm_sys::prelude::LLVMModuleRef;
use llvm_sys::target::{
  LLVMCopyStringRepOfTargetData, LLVMDisposeTargetData, LLVM_InitializeNativeAsmPrinter,
  LLVM_InitializeNativeTarget,
};
use llvm_sys::target_machine::{
  LLVMCodeGenFileType, LLVMCodeGenOptLevel, LLVMCodeModel, LLVMCreateTargetDataLayout,
  LLVMCreateTargetMachine, LLVMDisposeTargetMachine, LLVMGetDefaultTargetTriple,
  LLVMGetTargetFromTriple, LLVMRelocMode, LLVMTargetMachineEmitToMemoryBuffer,
};

use native_js::gc::statepoints::{LiveGcPtr, StatepointCallee, StatepointIntrinsics};

#[test]
fn statepoint_overloaded_intrinsics_are_canonically_mangled() {
  let ctx = Context::create();
  let module = ctx.create_module("statepoints_intrinsics");
  let builder = ctx.create_builder();

  let void_ty = ctx.void_type();
  let i1_ty = ctx.bool_type();
  let i64_ty = ctx.i64_type();
  let p1_ty = ctx.ptr_type(AddressSpace::from(1));
  let struct_ty = ctx.struct_type(
    &[i64_ty.as_basic_type_enum(), i64_ty.as_basic_type_enum()],
    false,
  );

  // Callees (no args).
  let callee_void = module.add_function("callee_void", void_ty.fn_type(&[], false), None);
  let callee_i1 = module.add_function("callee_i1", i1_ty.fn_type(&[], false), None);
  let callee_i64 = module.add_function("callee_i64", i64_ty.fn_type(&[], false), None);
  let callee_p1 = module.add_function("callee_p1", p1_ty.fn_type(&[], false), None);
  let callee_struct = module.add_function("callee_struct", struct_ty.fn_type(&[], false), None);

  // One function that exercises multiple `gc.result` overloads + one `gc.relocate` overload.
  let test_fn = module.add_function(
    "test_all",
    void_ty.fn_type(&[p1_ty.as_basic_type_enum().into()], false),
    None,
  );
  unsafe {
    // LLVM verifier requires a GC strategy on any function containing `gc.statepoint`.
    LLVMSetGC(test_fn.as_value_ref(), c"statepoint-example".as_ptr());
  }

  let entry = ctx.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);
  let live_ptr = test_fn
    .get_nth_param(0)
    .expect("missing live ptr param")
    .into_pointer_value();

  let mut sp = StatepointIntrinsics::new(&module);

  // void
  let callee = StatepointCallee::new(
    callee_void.as_global_value().as_pointer_value(),
    callee_void.get_type(),
  );
  let (_ret, _relocated) = sp.emit_statepoint_call(&builder, callee, &[], &[], None);

  // i1
  let callee = StatepointCallee::new(
    callee_i1.as_global_value().as_pointer_value(),
    callee_i1.get_type(),
  );
  let (_ret, _relocated) = sp.emit_statepoint_call(
    &builder,
    callee,
    &[],
    &[],
    Some(i1_ty.as_basic_type_enum()),
  );

  // i64
  let callee = StatepointCallee::new(
    callee_i64.as_global_value().as_pointer_value(),
    callee_i64.get_type(),
  );
  let (_ret, _relocated) = sp.emit_statepoint_call(
    &builder,
    callee,
    &[],
    &[],
    Some(i64_ty.as_basic_type_enum()),
  );

  // ptr addrspace(1)
  let callee = StatepointCallee::new(
    callee_p1.as_global_value().as_pointer_value(),
    callee_p1.get_type(),
  );
  let (_ret, _relocated) = sp.emit_statepoint_call(
    &builder,
    callee,
    &[],
    &[],
    Some(p1_ty.as_basic_type_enum()),
  );

  // { i64, i64 }
  let callee = StatepointCallee::new(
    callee_struct.as_global_value().as_pointer_value(),
    callee_struct.get_type(),
  );
  let (_ret, _relocated) = sp.emit_statepoint_call(
    &builder,
    callee,
    &[],
    &[],
    Some(struct_ty.as_basic_type_enum()),
  );

  // gc.relocate.p1 (uses a gc-live operand bundle).
  let callee = StatepointCallee::new(
    callee_void.as_global_value().as_pointer_value(),
    callee_void.get_type(),
  );
  let (_ret, relocated) =
    sp.emit_statepoint_call(&builder, callee, &[], &[LiveGcPtr::new(live_ptr)], None);
  assert_eq!(relocated.len(), 1, "expected one relocated pointer");

  builder.build_return(None).expect("build return");

  unsafe {
    let module_ref = module.as_mut_ptr();
    verify(module_ref);
    let ir = module_to_string(module_ref);

    assert!(ir.contains("@llvm.experimental.gc.result.i1("), "{ir}");
    assert!(ir.contains("@llvm.experimental.gc.result.i64("), "{ir}");
    assert!(ir.contains("@llvm.experimental.gc.result.p1("), "{ir}");
    assert!(ir.contains("@llvm.experimental.gc.result.sl_i64i64s("), "{ir}");
    assert!(ir.contains("@llvm.experimental.gc.relocate.p1("), "{ir}");

    // Ensure the module is actually codegen-able under LLVM 18.
    emit_object(module_ref);
  }
}

unsafe fn verify(module: LLVMModuleRef) {
  let mut err = std::ptr::null_mut();
  let status = LLVMVerifyModule(
    module,
    LLVMVerifierFailureAction::LLVMReturnStatusAction,
    &mut err,
  );
  if status != 0 {
    let msg = if err.is_null() {
      "<no verifier message>".to_string()
    } else {
      CStr::from_ptr(err).to_string_lossy().into_owned()
    };
    if !err.is_null() {
      LLVMDisposeMessage(err);
    }
    panic!("LLVMVerifyModule failed:\n{msg}");
  }
  if !err.is_null() {
    LLVMDisposeMessage(err);
  }
}

unsafe fn module_to_string(module: LLVMModuleRef) -> String {
  let s = LLVMPrintModuleToString(module);
  let out = CStr::from_ptr(s).to_string_lossy().into_owned();
  LLVMDisposeMessage(s);
  out
}

unsafe fn emit_object(module: LLVMModuleRef) {
  // The initialize APIs are idempotent.
  LLVM_InitializeNativeTarget();
  LLVM_InitializeNativeAsmPrinter();

  let triple = LLVMGetDefaultTargetTriple();
  LLVMSetTarget(module, triple);

  let mut target = std::ptr::null_mut();
  let mut err = std::ptr::null_mut();
  if LLVMGetTargetFromTriple(triple, &mut target, &mut err) != 0 {
    let msg = if err.is_null() {
      "<no error message>".to_string()
    } else {
      CStr::from_ptr(err).to_string_lossy().into_owned()
    };
    if !err.is_null() {
      LLVMDisposeMessage(err);
    }
    LLVMDisposeMessage(triple);
    panic!("LLVMGetTargetFromTriple failed: {msg}");
  }

  let cpu = CString::new("generic").unwrap();
  let features = CString::new("").unwrap();
  let tm = LLVMCreateTargetMachine(
    target,
    triple,
    cpu.as_ptr(),
    features.as_ptr(),
    LLVMCodeGenOptLevel::LLVMCodeGenLevelDefault,
    LLVMRelocMode::LLVMRelocDefault,
    LLVMCodeModel::LLVMCodeModelDefault,
  );
  assert!(!tm.is_null(), "LLVMCreateTargetMachine returned null");

  let dl = LLVMCreateTargetDataLayout(tm);
  let dl_str = LLVMCopyStringRepOfTargetData(dl);
  LLVMSetDataLayout(module, dl_str);
  LLVMDisposeMessage(dl_str);
  LLVMDisposeTargetData(dl);

  let mut out = std::ptr::null_mut();
  let mut emit_err = std::ptr::null_mut();
  let status = LLVMTargetMachineEmitToMemoryBuffer(
    tm,
    module,
    LLVMCodeGenFileType::LLVMObjectFile,
    &mut emit_err,
    &mut out,
  );
  if status != 0 {
    let msg = if emit_err.is_null() {
      "<no error message>".to_string()
    } else {
      CStr::from_ptr(emit_err).to_string_lossy().into_owned()
    };
    if !emit_err.is_null() {
      LLVMDisposeMessage(emit_err);
    }
    LLVMDisposeTargetMachine(tm);
    LLVMDisposeMessage(triple);
    panic!("LLVMTargetMachineEmitToMemoryBuffer failed: {msg}");
  }

  llvm_sys::core::LLVMDisposeMemoryBuffer(out);
  LLVMDisposeTargetMachine(tm);
  LLVMDisposeMessage(triple);
}

