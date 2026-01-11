use inkwell::module::Module;
use llvm_sys::core::{
  LLVMCountBasicBlocks, LLVMDisposeMessage, LLVMGetAllocatedType, LLVMGetFirstBasicBlock,
  LLVMGetFirstFunction, LLVMGetFirstInstruction, LLVMGetFirstUse, LLVMGetGC, LLVMGetNextBasicBlock,
  LLVMGetNextFunction, LLVMGetNextInstruction, LLVMGetNextUse, LLVMGetNumOperands, LLVMGetOperand,
  LLVMGetPointerAddressSpace, LLVMGetTypeKind, LLVMGetUser, LLVMPrintValueToString, LLVMTypeOf,
};
use llvm_sys::prelude::{LLVMModuleRef, LLVMTypeRef, LLVMUseRef, LLVMValueRef};
use llvm_sys::{LLVMOpcode, LLVMTypeKind};
use std::collections::HashSet;
use std::ffi::CStr;
use std::fmt;

use super::gc::GC_ADDR_SPACE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintRule {
  /// Rule A: Forbid `addrspacecast` to/from `addrspace(1)` in non-runtime wrapper GC-managed
  /// functions.
  NonWrapperAddrSpaceCastToOrFromGcPointer,
  /// Rule B1: In runtime ABI wrapper functions, `addrspacecast` from AS0->AS1 must be returned or
  /// stored into an AS1 pointer slot.
  WrapperAddrSpaceCastAs0ToAs1InvalidUse,
  /// Rule B2/B3: In runtime ABI wrapper functions, `addrspacecast` from AS1->AS0 is forbidden.
  ///
  /// `native-js`'s GC pointer discipline does not allow producing addrspace(0) aliases of GC
  /// pointers, since `rewrite-statepoints-for-gc` will not relocate them. Runtime wrappers should
  /// use indirect calls with addrspace(1) signatures instead of casting GC pointers to AS0.
  WrapperAddrSpaceCastAs1ToAs0InvalidUse,
  /// Rule B: Runtime ABI wrappers may only cast between AS0 and AS1 (not other addrspaces).
  WrapperAddrSpaceCastBetweenUnsupportedAddrSpaces,
  /// Rule A: forbid `ptrtoint` from `ptr addrspace(1)`.
  PtrToIntFromGcPointer,
  /// Rule A: forbid `inttoptr` to `ptr addrspace(1)`.
  IntToPtrToGcPointer,
  /// Rule A: forbid obvious stores of addrspace(1) pointers into non-pointer-typed slots.
  StoreGcPointerToNonPointerSlot,
  /// Rule A: forbid returning `ptr` (addrspace(0)) derived from `ptr addrspace(1)`.
  ReturnAddrSpace0PointerDerivedFromGcPointer,
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

/// Enforce a conservative LLVM GC pointer discipline for statepoint-based moving GC.
///
/// ## Why this exists
/// LLVM's `rewrite-statepoints-for-gc` pass only relocates SSA values of type
/// `ptr addrspace(1)`. If a GC pointer is "hidden" by converting it to `ptr`
/// (addrspace(0)), integer types (`ptrtoint`), or other non-tracked forms, it
/// will not be updated across safepoints.
///
/// This lint is intentionally conservative. It is meant to catch *obviously* unsound IR patterns
/// early (during debug builds/tests) rather than rely on subtle invariants.
pub fn lint_module_gc_pointer_discipline(module: &Module<'_>) -> Result<(), LintError> {
  // Use llvm-sys to iterate and inspect instructions; inkwell's iterator APIs and instruction
  // wrappers have historically changed with LLVM versions.
  unsafe { lint_module_gc_pointer_discipline_raw(module.as_mut_ptr()) }
}

unsafe fn lint_module_gc_pointer_discipline_raw(module: LLVMModuleRef) -> Result<(), LintError> {
  assert!(!module.is_null(), "module must be non-null");

  let mut violations = Vec::<LintViolation>::new();

  let mut func = LLVMGetFirstFunction(module);
  while !func.is_null() {
    if is_gc_managed_function(func) {
      let func_name = value_name(func);
      let is_wrapper = is_runtime_abi_wrapper_function_name(&func_name);
      lint_instructions_in_function(func, func_name.as_str(), is_wrapper, &mut violations);
    }

    func = LLVMGetNextFunction(func);
  }

  if violations.is_empty() {
    Ok(())
  } else {
    Err(LintError { violations })
  }
}

/// `true` iff LLVM considers this function GC-managed (i.e. has a `gc "<strategy>"` attribute).
///
/// The strategy name itself isn't relevant to the pointer discipline; we only care that LLVM will
/// run `rewrite-statepoints-for-gc` and use stack maps/statepoints semantics for the function.
unsafe fn is_gc_managed_function(func: LLVMValueRef) -> bool {
  !LLVMGetGC(func).is_null()
}

/// Identify internal runtime ABI wrapper functions.
///
/// These wrappers intentionally contain `addrspacecast` between AS0 (raw pointers used by the Rust
/// runtime ABI) and AS1 (GC pointers tracked by LLVM statepoints).
///
/// We centralize the naming convention here so codegen cannot accidentally (or intentionally)
/// bypass the wrapper-only allowances in this lint: if a function wants to use AS0⇄AS1 casts, it
/// **must** be named like `rt_*_gc`.
fn is_runtime_abi_wrapper_function_name(name: &str) -> bool {
  name.starts_with("rt_") && name.ends_with("_gc")
}

unsafe fn lint_instructions_in_function(
  func: LLVMValueRef,
  func_name: &str,
  is_wrapper: bool,
  violations: &mut Vec<LintViolation>,
) {
  // Skip declarations.
  if LLVMCountBasicBlocks(func) == 0 {
    return;
  }

  let mut bb = LLVMGetFirstBasicBlock(func);
  while !bb.is_null() {
    let mut inst = LLVMGetFirstInstruction(bb);
    while !inst.is_null() {
      lint_instruction(func_name, is_wrapper, inst, violations);
      inst = LLVMGetNextInstruction(inst);
    }

    bb = LLVMGetNextBasicBlock(bb);
  }
}

unsafe fn lint_instruction(
  func_name: &str,
  is_wrapper: bool,
  inst: LLVMValueRef,
  violations: &mut Vec<LintViolation>,
) {
  let opcode = llvm_sys::core::LLVMGetInstructionOpcode(inst);

  match opcode {
    LLVMOpcode::LLVMPtrToInt => lint_ptrtoint(func_name, inst, violations),
    LLVMOpcode::LLVMIntToPtr => lint_inttoptr(func_name, inst, violations),
    LLVMOpcode::LLVMAddrSpaceCast => lint_addrspacecast(func_name, is_wrapper, inst, violations),
    LLVMOpcode::LLVMStore => lint_store(func_name, inst, violations),
    LLVMOpcode::LLVMRet => {
      if !is_wrapper {
        lint_return_raw_pointer(func_name, inst, violations);
      }
    }
    _ => {}
  }
}

unsafe fn lint_ptrtoint(func_name: &str, inst: LLVMValueRef, violations: &mut Vec<LintViolation>) {
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

unsafe fn lint_inttoptr(func_name: &str, inst: LLVMValueRef, violations: &mut Vec<LintViolation>) {
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

unsafe fn lint_addrspacecast(
  func_name: &str,
  is_wrapper: bool,
  inst: LLVMValueRef,
  violations: &mut Vec<LintViolation>,
) {
  let operand = LLVMGetOperand(inst, 0);
  let operand_ty = LLVMTypeOf(operand);
  let result_ty = LLVMTypeOf(inst);

  if !is_pointer_type(operand_ty) || !is_pointer_type(result_ty) {
    return;
  }

  let src_as = LLVMGetPointerAddressSpace(operand_ty);
  let dst_as = LLVMGetPointerAddressSpace(result_ty);

  // Only care about casts involving AS1 (GC pointers).
  if src_as != GC_ADDR_SPACE && dst_as != GC_ADDR_SPACE {
    return;
  }

  if !is_wrapper {
    violations.push(LintViolation {
      rule: LintRule::NonWrapperAddrSpaceCastToOrFromGcPointer,
      message: format!(
        "in `{}`: disallowed `addrspacecast` to/from addrspace(1) in non-wrapper function: {}",
        func_name,
        value_to_string(inst)
      ),
    });
    return;
  }

  if src_as == 0 && dst_as == GC_ADDR_SPACE {
    lint_wrapper_as0_to_as1_cast(func_name, inst, violations);
  } else if src_as == GC_ADDR_SPACE && dst_as == 0 {
    lint_wrapper_as1_to_as0_cast(func_name, inst, violations);
  } else {
    violations.push(LintViolation {
      rule: LintRule::WrapperAddrSpaceCastBetweenUnsupportedAddrSpaces,
      message: format!(
        "in `{}`: runtime ABI wrapper may only `addrspacecast` between AS0 and AS1, got AS{}->AS{}: {}",
        func_name,
        src_as,
        dst_as,
        value_to_string(inst)
      ),
    });
  }
}

unsafe fn lint_wrapper_as0_to_as1_cast(
  func_name: &str,
  cast: LLVMValueRef,
  violations: &mut Vec<LintViolation>,
) {
  // Allowed uses:
  //   - returned
  //   - stored into a pointer-typed slot whose element type is `ptr addrspace(1)`
  let mut use_ref: LLVMUseRef = LLVMGetFirstUse(cast);
  while !use_ref.is_null() {
    let user = LLVMGetUser(use_ref);
    if llvm_sys::core::LLVMIsAInstruction(user).is_null() {
      violations.push(LintViolation {
        rule: LintRule::WrapperAddrSpaceCastAs0ToAs1InvalidUse,
        message: format!(
          "in `{}`: AS0->AS1 cast used by non-instruction user: {}",
          func_name,
          value_to_string(cast)
        ),
      });
      use_ref = LLVMGetNextUse(use_ref);
      continue;
    }

    match llvm_sys::core::LLVMGetInstructionOpcode(user) {
      LLVMOpcode::LLVMRet => {
        // OK.
      }

      LLVMOpcode::LLVMStore => {
        // Operand 0: stored value, operand 1: destination address.
        let stored = LLVMGetOperand(user, 0);
        if stored != cast {
          violations.push(LintViolation {
            rule: LintRule::WrapperAddrSpaceCastAs0ToAs1InvalidUse,
            message: format!(
              "in `{}`: AS0->AS1 cast used as store destination (must be stored value): {}",
              func_name,
              value_to_string(user)
            ),
          });
          use_ref = LLVMGetNextUse(use_ref);
          continue;
        }

        let dest = LLVMGetOperand(user, 1);
        match known_memory_slot_type(dest) {
          Some(slot_ty) if is_gc_pointer_type(slot_ty) => {
            // OK.
          }
          Some(slot_ty) => violations.push(LintViolation {
            rule: LintRule::WrapperAddrSpaceCastAs0ToAs1InvalidUse,
            message: format!(
              "in `{}`: AS0->AS1 cast stored into slot of type `{}`; expected `ptr addrspace(1)` slot: {}",
              func_name,
              type_to_string(slot_ty),
              value_to_string(user)
            ),
          }),
          None => violations.push(LintViolation {
            rule: LintRule::WrapperAddrSpaceCastAs0ToAs1InvalidUse,
            message: format!(
              "in `{}`: AS0->AS1 cast stored into unknown slot type (expected `ptr addrspace(1)` slot): {}",
              func_name,
              value_to_string(user)
            ),
          }),
        }
      }

      _ => violations.push(LintViolation {
        rule: LintRule::WrapperAddrSpaceCastAs0ToAs1InvalidUse,
        message: format!(
          "in `{}`: AS0->AS1 cast must be returned or stored into AS1 pointer slot, but used by: {}",
          func_name,
          value_to_string(user)
        ),
      }),
    }

    use_ref = LLVMGetNextUse(use_ref);
  }
}

unsafe fn lint_wrapper_as1_to_as0_cast(
  func_name: &str,
  cast: LLVMValueRef,
  violations: &mut Vec<LintViolation>,
) {
  // Disallowed: addrspacecast from AS1->AS0.
  //
  // Even in runtime wrappers this is unsound: `rewrite-statepoints-for-gc` only relocates
  // `ptr addrspace(1)` SSA values, so creating an AS0 alias risks keeping the alias live across a
  // safepoint while the AS1 value is dead.
  violations.push(LintViolation {
    rule: LintRule::WrapperAddrSpaceCastAs1ToAs0InvalidUse,
    message: format!(
      "in `{}`: disallowed `addrspacecast` from addrspace(1) to addrspace(0): {}",
      func_name,
      value_to_string(cast)
    ),
  });
}

unsafe fn lint_store(func_name: &str, inst: LLVMValueRef, violations: &mut Vec<LintViolation>) {
  // Operand 0: stored value, operand 1: destination address.
  if LLVMGetNumOperands(inst) < 2 {
    return;
  }

  let stored = LLVMGetOperand(inst, 0);
  let stored_ty = LLVMTypeOf(stored);
  if !is_gc_pointer_type(stored_ty) {
    return;
  }

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

unsafe fn lint_return_raw_pointer(func_name: &str, inst: LLVMValueRef, violations: &mut Vec<LintViolation>) {
  // Operand 0: return value (absent for `ret void`).
  if LLVMGetNumOperands(inst) == 0 {
    return;
  }

  let ret_val = LLVMGetOperand(inst, 0);
  let ret_ty = LLVMTypeOf(ret_val);
  if !is_pointer_type(ret_ty) || LLVMGetPointerAddressSpace(ret_ty) != 0 {
    return;
  }

  if is_addrspace0_pointer_derived_from_gc_pointer(ret_val) {
    violations.push(LintViolation {
      rule: LintRule::ReturnAddrSpace0PointerDerivedFromGcPointer,
      message: format!(
        "in `{}`: disallowed return of addrspace(0) pointer derived from GC pointer: {}",
        func_name,
        value_to_string(inst)
      ),
    });
  }
}

unsafe fn is_addrspace0_pointer_derived_from_gc_pointer(val: LLVMValueRef) -> bool {
  let mut visited = HashSet::<LLVMValueRef>::new();
  is_addrspace0_pointer_derived_from_gc_pointer_inner(val, &mut visited)
}

unsafe fn is_addrspace0_pointer_derived_from_gc_pointer_inner(
  val: LLVMValueRef,
  visited: &mut HashSet<LLVMValueRef>,
) -> bool {
  if val.is_null() || visited.contains(&val) {
    return false;
  }
  visited.insert(val);

  let ty = LLVMTypeOf(val);
  if is_gc_pointer_type(ty) {
    return true;
  }

  // Only treat a narrow set of pointer->pointer SSA operations as "deriving" a pointer from
  // another. This intentionally avoids treating `load ptr ...` as derived, since that may be a
  // raw pointer field within a GC object.
  if llvm_sys::core::LLVMIsAInstruction(val).is_null() {
    return false;
  }

  match llvm_sys::core::LLVMGetInstructionOpcode(val) {
    LLVMOpcode::LLVMAddrSpaceCast
    | LLVMOpcode::LLVMBitCast
    | LLVMOpcode::LLVMGetElementPtr
    | LLVMOpcode::LLVMPHI
    | LLVMOpcode::LLVMSelect => {
      let num_ops = LLVMGetNumOperands(val);
      for i in 0..num_ops {
        let op = LLVMGetOperand(val, i as u32);
        if is_addrspace0_pointer_derived_from_gc_pointer_inner(op, visited) {
          return true;
        }
      }
      false
    }
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
