//! Helpers for running LLVM passes on generated modules.
//!
//! The native backend uses LLVM's new pass manager via the C API `LLVMRunPasses`.
//! We keep the wrapper in this crate so future codegen can run safepoint/statepoint
//! pipelines without shelling out to `opt`.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use llvm_sys::core::{
  LLVMAddFunction, LLVMDisposeMessage, LLVMFunctionType, LLVMGetModuleContext, LLVMGetNamedFunction,
  LLVMVoidTypeInContext,
};
use llvm_sys::prelude::{LLVMContextRef, LLVMModuleRef};
use llvm_sys::target::{
  LLVM_InitializeNativeAsmParser, LLVM_InitializeNativeAsmPrinter, LLVM_InitializeNativeTarget,
};
use llvm_sys::target_machine::{
  LLVMCodeGenOptLevel, LLVMCodeModel, LLVMCreateTargetMachine, LLVMGetDefaultTargetTriple,
  LLVMGetTargetFromTriple, LLVMRelocMode, LLVMTargetMachineRef,
};

extern "C" {
  fn LLVMCreatePassBuilderOptions() -> *mut c_void;
  fn LLVMDisposePassBuilderOptions(options: *mut c_void);
  fn LLVMRunPasses(
    module: LLVMModuleRef,
    passes: *const c_char,
    target_machine: LLVMTargetMachineRef,
    options: *mut c_void,
  ) -> *mut c_void; // LLVMErrorRef

  fn LLVMGetErrorMessage(err: *mut c_void) -> *mut c_char;
  fn LLVMDisposeErrorMessage(msg: *mut c_char);
  fn LLVMConsumeError(err: *mut c_void);

  fn LLVMCreateMemoryBufferWithMemoryRangeCopy(
    input_data: *const c_char,
    input_data_length: usize,
    buffer_name: *const c_char,
  ) -> *mut c_void; // LLVMMemoryBufferRef
  fn LLVMDisposeMemoryBuffer(mem_buf: *mut c_void);
  fn LLVMParseIRInContext(
    context: LLVMContextRef,
    mem_buf: *mut c_void,
    out_module: *mut LLVMModuleRef,
    out_message: *mut *mut c_char,
  ) -> i32; // LLVMBool
}

/// Ensure the module contains the `gc.safepoint_poll` declaration that LLVM's
/// `place-safepoints` pass expects to exist.
///
/// On Ubuntu LLVM 18.1.3, `place-safepoints` can segfault when it tries to
/// materialize this declaration itself. Predeclaring it avoids that crash.
pub unsafe fn ensure_gc_safepoint_poll_decl(module: LLVMModuleRef) {
  let name = CString::new("gc.safepoint_poll").expect("CString");
  if !LLVMGetNamedFunction(module, name.as_ptr()).is_null() {
    return;
  }

  let ctx = LLVMGetModuleContext(module);
  let void_ty = LLVMVoidTypeInContext(ctx);
  let fn_ty = LLVMFunctionType(void_ty, std::ptr::null_mut(), 0, 0);
  LLVMAddFunction(module, name.as_ptr(), fn_ty);
}

/// Run a new-PM pass pipeline via LLVM's C API.
pub unsafe fn run_pass_pipeline(
  module: LLVMModuleRef,
  target_machine: LLVMTargetMachineRef,
  pipeline: &CStr,
) -> Result<(), String> {
  let options = LLVMCreatePassBuilderOptions();
  if options.is_null() {
    return Err("LLVMCreatePassBuilderOptions returned null".to_string());
  }

  let err = LLVMRunPasses(module, pipeline.as_ptr(), target_machine, options);
  LLVMDisposePassBuilderOptions(options);

  if err.is_null() {
    return Ok(());
  }

  let msg_ptr = LLVMGetErrorMessage(err);
  let msg = if msg_ptr.is_null() {
    "LLVMRunPasses returned an error (no message)".to_string()
  } else {
    CStr::from_ptr(msg_ptr).to_string_lossy().into_owned()
  };
  LLVMDisposeErrorMessage(msg_ptr);
  LLVMConsumeError(err);
  Err(msg)
}

/// Convenience wrapper for the safepoint pipeline used by the runtime:
///
/// 1. Insert safepoint polls (`place-safepoints`, function pass)
/// 2. Rewrite calls into statepoints (`rewrite-statepoints-for-gc`, module pass)
pub unsafe fn run_place_safepoints_and_rewrite_statepoints_for_gc(
  module: LLVMModuleRef,
  target_machine: LLVMTargetMachineRef,
) -> Result<(), String> {
  ensure_gc_safepoint_poll_decl(module);
  let pipeline =
    CString::new("function(place-safepoints),rewrite-statepoints-for-gc").expect("CString");
  run_pass_pipeline(module, target_machine, &pipeline)
}

/// Create a target machine for the current host.
///
/// Note: callers are responsible for disposing the returned target machine with
/// `LLVMDisposeTargetMachine`.
pub unsafe fn create_host_target_machine() -> Result<LLVMTargetMachineRef, String> {
  // Safe to call multiple times; LLVM internally guards initialization.
  LLVM_InitializeNativeTarget();
  LLVM_InitializeNativeAsmPrinter();
  LLVM_InitializeNativeAsmParser();

  let triple = LLVMGetDefaultTargetTriple();
  if triple.is_null() {
    return Err("LLVMGetDefaultTargetTriple returned null".to_string());
  }

  let mut target = std::mem::MaybeUninit::uninit();
  let mut err_msg: *mut c_char = std::ptr::null_mut();
  let ok = LLVMGetTargetFromTriple(triple, target.as_mut_ptr(), &mut err_msg);
  if ok != 0 {
    let msg = if err_msg.is_null() {
      "LLVMGetTargetFromTriple failed (no message)".to_string()
    } else {
      CStr::from_ptr(err_msg).to_string_lossy().into_owned()
    };
    LLVMDisposeMessage(err_msg);
    LLVMDisposeMessage(triple);
    return Err(msg);
  }

  let cpu = CString::new("").expect("CString");
  let features = CString::new("").expect("CString");
  let tm = LLVMCreateTargetMachine(
    unsafe { target.assume_init() },
    triple,
    cpu.as_ptr(),
    features.as_ptr(),
    LLVMCodeGenOptLevel::LLVMCodeGenLevelDefault,
    LLVMRelocMode::LLVMRelocDefault,
    LLVMCodeModel::LLVMCodeModelDefault,
  );

  LLVMDisposeMessage(triple);

  if tm.is_null() {
    return Err("LLVMCreateTargetMachine returned null".to_string());
  }
  Ok(tm)
}

#[cfg(test)]
mod tests {
  use super::*;
  use llvm_sys::core::{
    LLVMContextCreate, LLVMContextDispose, LLVMDisposeModule, LLVMPrintModuleToString,
  };
  use llvm_sys::target_machine::LLVMDisposeTargetMachine;

  unsafe fn parse_ir(ir: &str) -> Result<(LLVMContextRef, LLVMModuleRef), String> {
    let ctx = LLVMContextCreate();
    if ctx.is_null() {
      return Err("LLVMContextCreate returned null".to_string());
    }

    let name = CString::new("test.ll").expect("CString");
    let buf = LLVMCreateMemoryBufferWithMemoryRangeCopy(
      ir.as_ptr() as *const c_char,
      ir.len(),
      name.as_ptr(),
    );
    if buf.is_null() {
      LLVMContextDispose(ctx);
      return Err("LLVMCreateMemoryBufferWithMemoryRangeCopy returned null".to_string());
    }

    let mut module: LLVMModuleRef = std::ptr::null_mut();
    let mut msg: *mut c_char = std::ptr::null_mut();
    let parse_failed = LLVMParseIRInContext(ctx, buf, &mut module, &mut msg);
    if parse_failed != 0 {
      let message = if msg.is_null() {
        "LLVMParseIRInContext failed (no message)".to_string()
      } else {
        CStr::from_ptr(msg).to_string_lossy().into_owned()
      };
      LLVMDisposeMessage(msg);
      LLVMDisposeMemoryBuffer(buf);
      LLVMContextDispose(ctx);
      return Err(message);
    }

    // `LLVMParseIRInContext` takes ownership of `buf` on success.
    Ok((ctx, module))
  }

  unsafe fn module_to_string(module: LLVMModuleRef) -> String {
    let ptr = LLVMPrintModuleToString(module);
    if ptr.is_null() {
      return "<null>".to_string();
    }
    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    LLVMDisposeMessage(ptr);
    s
  }

  #[test]
  fn place_safepoints_requires_predeclared_poll_on_llvm18() {
    // This IR intentionally omits a `declare void @gc.safepoint_poll()`; LLVM 18.1.3
    // can segfault if `place-safepoints` tries to create it itself.
    let ir = r#"
      source_filename = "njs_place_safepoints"

      define void @foo() gc "statepoint-example" {
      entry:
        ret void
      }
    "#;

    unsafe {
      let (ctx, module) = parse_ir(ir).expect("parse IR");
      let tm = create_host_target_machine().expect("target machine");

      run_place_safepoints_and_rewrite_statepoints_for_gc(module, tm)
        .expect("run safepoint + statepoint passes");

      let out = module_to_string(module);
      assert!(
        out.contains("gc.safepoint_poll"),
        "expected poll decl/call in output IR:\n{out}"
      );
      assert!(
        out.contains("llvm.experimental.gc.statepoint"),
        "expected statepoint in output IR:\n{out}"
      );

      LLVMDisposeTargetMachine(tm);
      LLVMDisposeModule(module);
      LLVMContextDispose(ctx);
    }
  }
}
