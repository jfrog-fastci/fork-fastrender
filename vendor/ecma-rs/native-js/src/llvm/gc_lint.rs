use llvm_sys::core::{
  LLVMCountBasicBlocks, LLVMCountParams, LLVMDisposeMessage, LLVMGetAllocatedType,
  LLVMGetFirstBasicBlock, LLVMGetFirstFunction, LLVMGetFirstInstruction, LLVMGetGC,
  LLVMGetNextBasicBlock, LLVMGetNextFunction, LLVMGetNextInstruction, LLVMGetNumOperands,
  LLVMGetOperand, LLVMGetParam, LLVMGetPointerAddressSpace, LLVMGetReturnType, LLVMGetTypeKind,
  LLVMPrintValueToString, LLVMTypeOf,
};
use llvm_sys::prelude::{LLVMModuleRef, LLVMTypeRef, LLVMValueRef};
use llvm_sys::{LLVMOpcode, LLVMTypeKind};
use std::ffi::CStr;
use std::fmt;

use super::gc::{GC_ADDR_SPACE, GC_STRATEGY};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintRule {
  /// Rule 1: under our GC strategy (`native_js::llvm::gc::GC_STRATEGY`), all pointer
  /// params/returns must be `ptr addrspace(1)`.
  GcFunctionSignatureUsesNonGcPointer,
  /// Rule 2: forbid `ptrtoint` from `ptr addrspace(1)`.
  PtrToIntFromGcPointer,
  /// Rule 2: forbid `inttoptr` to `ptr addrspace(1)`.
  IntToPtrToGcPointer,
  /// Rule 3: forbid `addrspacecast` from addrspace(1) to any other addrspace.
  AddrSpaceCastFromGcPointer,
  /// Rule 4: forbid obvious stores of addrspace(1) pointers into non-pointer-typed slots.
  StoreGcPointerToNonPointerSlot,
}

#[derive(Debug, Clone)]
pub struct LintViolation {
  pub rule: LintRule,
  pub message: String,
}

#[derive(Debug)]
pub struct LintError {
  pub violations: Vec<LintViolation>,
}

impl LintError {
  pub fn has_rule(&self, rule: LintRule) -> bool {
    self.violations.iter().any(|v| v.rule == rule)
  }
}

impl fmt::Display for LintError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    writeln!(
      f,
      "GC pointer discipline lint failed ({} violation(s)):",
      self.violations.len()
    )?;
    for v in &self.violations {
      writeln!(f, "- {:?}: {}", v.rule, v.message)?;
    }
    Ok(())
  }
}

impl std::error::Error for LintError {}

/// Enforce a conservative LLVM GC pointer discipline for our GC strategy.
///
/// ## Why this exists
/// LLVM's `rewrite-statepoints-for-gc` pass only relocates SSA values of type `ptr addrspace(1)`.
/// If a GC pointer is "hidden" by converting it to `ptr` (addrspace(0)), integer types
/// (`ptrtoint`), or other non-tracked forms, it will not be updated across safepoints.
///
/// This lint is intentionally conservative. It is meant to catch *obviously* unsound IR patterns
/// early (during debug builds/tests) rather than rely on subtle invariants.
pub fn lint_gc_pointer_discipline(module: LLVMModuleRef) -> Result<(), LintError> {
  assert!(!module.is_null(), "module must be non-null");

  let mut violations = Vec::<LintViolation>::new();

  unsafe {
    // Rule 1: GC functions must use `ptr addrspace(1)` in their signature.
    let mut func = LLVMGetFirstFunction(module);
    while !func.is_null() {
      if is_native_js_gc_function(func) {
        lint_gc_function_signature(func, &mut violations);
      }

      // Rule 2-4: scan all instructions (even outside GC functions). If a value is in AS1, it is
      // by definition a GC-managed pointer in our discipline.
      lint_instructions_in_function(func, &mut violations);

      func = LLVMGetNextFunction(func);
    }
  }

  if violations.is_empty() {
    Ok(())
  } else {
    Err(LintError { violations })
  }
}

unsafe fn lint_gc_function_signature(func: LLVMValueRef, violations: &mut Vec<LintViolation>) {
  let func_name = value_name(func);

  // `LLVMGetReturnType` expects an LLVMTypeRef of *function type*.
  //
  // With opaque pointers, `LLVMTypeOf(func)` is no longer a reliably useful way to recover the
  // function signature (it can just be `ptr`). Use `LLVMGlobalGetValueType` instead.
  let fn_ty = llvm_sys::core::LLVMGlobalGetValueType(func);
  let ret_ty = LLVMGetReturnType(fn_ty);
  if is_pointer_type(ret_ty) && !is_gc_pointer_type(ret_ty) {
    violations.push(LintViolation {
      rule: LintRule::GcFunctionSignatureUsesNonGcPointer,
      message: format!(
        "function `{}` has return type `{}` under `gc \"{}\"`; expected `ptr addrspace(1)`",
        func_name,
        type_to_string(ret_ty),
        GC_STRATEGY
      ),
    });
  }

  let param_count = LLVMCountParams(func);
  for i in 0..param_count {
    let param = LLVMGetParam(func, i);
    let param_ty = LLVMTypeOf(param);
    if is_pointer_type(param_ty) && !is_gc_pointer_type(param_ty) {
      violations.push(LintViolation {
        rule: LintRule::GcFunctionSignatureUsesNonGcPointer,
        message: format!(
          "function `{}` param #{} has type `{}` under `gc \"{}\"`; expected `ptr addrspace(1)`",
          func_name,
          i,
          type_to_string(param_ty),
          GC_STRATEGY
        ),
      });
    }
  }
}

unsafe fn lint_instructions_in_function(func: LLVMValueRef, violations: &mut Vec<LintViolation>) {
  let func_name = value_name(func);

  // Skip declarations.
  if LLVMCountBasicBlocks(func) == 0 {
    return;
  }

  let mut bb = LLVMGetFirstBasicBlock(func);
  while !bb.is_null() {
    let mut inst = LLVMGetFirstInstruction(bb);
    while !inst.is_null() {
      lint_instruction(func_name.as_str(), inst, violations);
      inst = LLVMGetNextInstruction(inst);
    }

    bb = LLVMGetNextBasicBlock(bb);
  }
}

unsafe fn lint_instruction(
  func_name: &str,
  inst: LLVMValueRef,
  violations: &mut Vec<LintViolation>,
) {
  let opcode = llvm_sys::core::LLVMGetInstructionOpcode(inst);

  match opcode {
    LLVMOpcode::LLVMPtrToInt => {
      let operand = LLVMGetOperand(inst, 0);
      let operand_ty = LLVMTypeOf(operand);
      if is_gc_pointer_type(operand_ty) {
        violations.push(LintViolation {
          rule: LintRule::PtrToIntFromGcPointer,
          message: format!(
            "in `{}`: disallowed `ptrtoint` of GC pointer: {}",
            func_name,
            value_to_string(inst)
          ),
        });
      }
    }

    LLVMOpcode::LLVMIntToPtr => {
      let result_ty = LLVMTypeOf(inst);
      if is_gc_pointer_type(result_ty) {
        violations.push(LintViolation {
          rule: LintRule::IntToPtrToGcPointer,
          message: format!(
            "in `{}`: disallowed `inttoptr` to GC pointer: {}",
            func_name,
            value_to_string(inst)
          ),
        });
      }
    }

    LLVMOpcode::LLVMAddrSpaceCast => {
      let operand = LLVMGetOperand(inst, 0);
      let operand_ty = LLVMTypeOf(operand);
      let result_ty = LLVMTypeOf(inst);
      if is_gc_pointer_type(operand_ty)
        && is_pointer_type(result_ty)
        && LLVMGetPointerAddressSpace(result_ty) != GC_ADDR_SPACE
      {
        violations.push(LintViolation {
          rule: LintRule::AddrSpaceCastFromGcPointer,
          message: format!(
            "in `{}`: disallowed `addrspacecast` from addrspace(1): {}",
            func_name,
            value_to_string(inst)
          ),
        });
      }
    }

    LLVMOpcode::LLVMStore => {
      // Operand 0: stored value, operand 1: destination address.
      if LLVMGetNumOperands(inst) >= 2 {
        let stored = LLVMGetOperand(inst, 0);
        let stored_ty = LLVMTypeOf(stored);
        if is_gc_pointer_type(stored_ty) {
          let dest = LLVMGetOperand(inst, 1);
          if let Some(slot_ty) = known_memory_slot_type(dest) {
            if !is_pointer_type(slot_ty) {
              violations.push(LintViolation {
                rule: LintRule::StoreGcPointerToNonPointerSlot,
                message: format!(
                  "in `{}`: disallowed store of GC pointer into non-pointer slot `{}`: {}",
                  func_name,
                  type_to_string(slot_ty),
                  value_to_string(inst)
                ),
              });
            }
          }
        }
      }
    }

    _ => {}
  }
}

unsafe fn is_native_js_gc_function(func: LLVMValueRef) -> bool {
  let gc = LLVMGetGC(func);
  if gc.is_null() {
    return false;
  }
  match CStr::from_ptr(gc).to_str() {
    Ok(strategy) if strategy == GC_STRATEGY => true,
    _ => false,
  }
}

unsafe fn is_pointer_type(ty: LLVMTypeRef) -> bool {
  LLVMGetTypeKind(ty) == LLVMTypeKind::LLVMPointerTypeKind
}

unsafe fn is_gc_pointer_type(ty: LLVMTypeRef) -> bool {
  is_pointer_type(ty) && LLVMGetPointerAddressSpace(ty) == GC_ADDR_SPACE
}

unsafe fn known_memory_slot_type(ptr: LLVMValueRef) -> Option<LLVMTypeRef> {
  let mut cur = ptr;

  // Peel through trivial pointer casts. In opaque-pointer IR, `bitcast ptr -> ptr` is legal and
  // shows up in some transformations even though it is type-preserving.
  loop {
    if llvm_sys::core::LLVMIsAInstruction(cur).is_null() {
      break;
    }
    let opcode = llvm_sys::core::LLVMGetInstructionOpcode(cur);
    if opcode == LLVMOpcode::LLVMBitCast || opcode == LLVMOpcode::LLVMAddrSpaceCast {
      cur = LLVMGetOperand(cur, 0);
      continue;
    }
    break;
  }

  if !llvm_sys::core::LLVMIsAAllocaInst(cur).is_null() {
    return Some(LLVMGetAllocatedType(cur));
  }

  if !llvm_sys::core::LLVMIsAGlobalVariable(cur).is_null() {
    return Some(llvm_sys::core::LLVMGlobalGetValueType(cur));
  }

  if !llvm_sys::core::LLVMIsAGetElementPtrInst(cur).is_null() {
    // In LLVM >= 15 (opaque pointers), the C API exposes the element type of a GEP result even
    // though the pointer value itself is opaque.
    return gep_result_element_type(cur);
  }

  None
}

unsafe fn gep_result_element_type(gep: LLVMValueRef) -> Option<LLVMTypeRef> {
  let mut ty = llvm_sys::core::LLVMGetGEPSourceElementType(gep);
  if ty.is_null() {
    return None;
  }

  // Operand 0 is the base pointer; operands 1.. are indices.
  //
  // The first index (operand 1) only performs pointer arithmetic within the "outermost" object and
  // does not change the pointee type. The remaining indices refine into aggregates.
  let num_operands = LLVMGetNumOperands(gep);
  if num_operands <= 2 {
    return Some(ty);
  }

  for op_i in 2..num_operands {
    let idx = LLVMGetOperand(gep, op_i as u32);
    ty = match LLVMGetTypeKind(ty) {
      LLVMTypeKind::LLVMArrayTypeKind
      | LLVMTypeKind::LLVMVectorTypeKind
      | LLVMTypeKind::LLVMScalableVectorTypeKind => llvm_sys::core::LLVMGetElementType(ty),

      LLVMTypeKind::LLVMStructTypeKind => {
        if llvm_sys::core::LLVMIsAConstantInt(idx).is_null() {
          return None;
        }
        let field_i = llvm_sys::core::LLVMConstIntGetZExtValue(idx);
        llvm_sys::core::LLVMStructGetTypeAtIndex(ty, field_i as u32)
      }

      // Indexing into a non-aggregate doesn't refine the pointee type. This is unusual but can
      // occur for pointer arithmetic where the source element type is scalar.
      _ => ty,
    };
  }

  Some(ty)
}

unsafe fn value_name(val: LLVMValueRef) -> String {
  let mut len: usize = 0;
  let ptr = llvm_sys::core::LLVMGetValueName2(val, &mut len as *mut usize);
  if ptr.is_null() || len == 0 {
    return "<anon>".to_string();
  }
  // `LLVMGetValueName2` returns a non-null-terminated buffer.
  let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
  String::from_utf8_lossy(bytes).to_string()
}

unsafe fn value_to_string(val: LLVMValueRef) -> String {
  let s = LLVMPrintValueToString(val);
  if s.is_null() {
    return "<unprintable>".to_string();
  }
  let out = CStr::from_ptr(s).to_string_lossy().into_owned();
  LLVMDisposeMessage(s);
  out
}

unsafe fn type_to_string(ty: LLVMTypeRef) -> String {
  let s = llvm_sys::core::LLVMPrintTypeToString(ty);
  if s.is_null() {
    return "<unprintable>".to_string();
  }
  let out = CStr::from_ptr(s).to_string_lossy().into_owned();
  LLVMDisposeMessage(s);
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use llvm_sys::core::{
    LLVMContextCreate, LLVMContextDispose, LLVMCreateMemoryBufferWithMemoryRangeCopy,
    LLVMDisposeMemoryBuffer, LLVMDisposeModule,
  };
  use llvm_sys::ir_reader::LLVMParseIRInContext;
  use llvm_sys::prelude::LLVMContextRef;
  use std::ffi::CString;
  use std::os::raw::c_char;
  use std::ptr;

  struct ParsedModule {
    ctx: LLVMContextRef,
    module: LLVMModuleRef,
  }

  impl ParsedModule {
    fn parse(ir: &str) -> Self {
      unsafe {
        let ctx = LLVMContextCreate();
        let name = CString::new("test").unwrap();
        let buf = LLVMCreateMemoryBufferWithMemoryRangeCopy(
          ir.as_ptr() as *const c_char,
          ir.len(),
          name.as_ptr(),
        );

        let mut module = ptr::null_mut();
        let mut err = ptr::null_mut();
        let rc = LLVMParseIRInContext(ctx, buf, &mut module, &mut err);
        // `LLVMParseIRInContext` takes ownership of the memory buffer (it is freed by LLVM).
        // Disposing it here would double-free on some LLVM builds.

        if rc != 0 {
          // On failure LLVM does not take ownership of the buffer.
          LLVMDisposeMemoryBuffer(buf);
          let msg = if err.is_null() {
            "unknown parse error".to_string()
          } else {
            let out = CStr::from_ptr(err).to_string_lossy().into_owned();
            LLVMDisposeMessage(err);
            out
          };
          panic!("failed to parse IR: {msg}\n{ir}");
        }

        ParsedModule { ctx, module }
      }
    }
  }

  impl Drop for ParsedModule {
    fn drop(&mut self) {
      unsafe {
        LLVMDisposeModule(self.module);
        LLVMContextDispose(self.ctx);
      }
    }
  }

  #[test]
  fn good_module_passes() {
    let m = ParsedModule::parse(
      r#"
        source_filename = "test"

        define void @good(ptr addrspace(1) %p) gc "coreclr" {
        entry:
          %slot = alloca ptr addrspace(1)
          store ptr addrspace(1) %p, ptr %slot
          %q = load ptr addrspace(1), ptr %slot
          ret void
        }
      "#,
    );

    lint_gc_pointer_discipline(m.module).unwrap();
  }

  #[test]
  fn rejects_gc_function_signature_with_addrspace0_pointer() {
    let m = ParsedModule::parse(
      r#"
        source_filename = "test"

        define void @bad_sig(ptr %p) gc "coreclr" {
        entry:
          ret void
        }
      "#,
    );

    let err = lint_gc_pointer_discipline(m.module).unwrap_err();
    assert!(
      err.has_rule(LintRule::GcFunctionSignatureUsesNonGcPointer),
      "{err}"
    );
  }

  #[test]
  fn rejects_ptrtoint_from_addrspace1_pointer() {
    let m = ParsedModule::parse(
      r#"
        source_filename = "test"

        define i64 @bad_ptrtoint(ptr addrspace(1) %p) gc "coreclr" {
        entry:
          %i = ptrtoint ptr addrspace(1) %p to i64
          ret i64 %i
        }
      "#,
    );

    let err = lint_gc_pointer_discipline(m.module).unwrap_err();
    assert!(err.has_rule(LintRule::PtrToIntFromGcPointer), "{err}");
  }

  #[test]
  fn rejects_inttoptr_to_addrspace1_pointer() {
    let m = ParsedModule::parse(
      r#"
        source_filename = "test"

        define ptr addrspace(1) @bad_inttoptr(i64 %i) gc "coreclr" {
        entry:
          %p = inttoptr i64 %i to ptr addrspace(1)
          ret ptr addrspace(1) %p
        }
      "#,
    );

    let err = lint_gc_pointer_discipline(m.module).unwrap_err();
    assert!(err.has_rule(LintRule::IntToPtrToGcPointer), "{err}");
  }

  #[test]
  fn rejects_addrspacecast_from_addrspace1_pointer() {
    let m = ParsedModule::parse(
      r#"
        source_filename = "test"

        declare void @use(ptr)

        define void @bad_as_cast(ptr addrspace(1) %p) gc "coreclr" {
        entry:
          %q = addrspacecast ptr addrspace(1) %p to ptr
          call void @use(ptr %q)
          ret void
        }
      "#,
    );

    let err = lint_gc_pointer_discipline(m.module).unwrap_err();
    assert!(err.has_rule(LintRule::AddrSpaceCastFromGcPointer), "{err}");
  }

  #[test]
  fn rejects_store_of_addrspace1_pointer_into_i64_slot() {
    let m = ParsedModule::parse(
      r#"
        source_filename = "test"

        define void @bad_store(ptr addrspace(1) %p) gc "coreclr" {
        entry:
          %slot = alloca i64
          store ptr addrspace(1) %p, ptr %slot
          ret void
        }
      "#,
    );

    let err = lint_gc_pointer_discipline(m.module).unwrap_err();
    assert!(
      err.has_rule(LintRule::StoreGcPointerToNonPointerSlot),
      "{err}"
    );
  }
}
