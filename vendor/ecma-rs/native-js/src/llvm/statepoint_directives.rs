use llvm_sys::core::{
  LLVMAddCallSiteAttribute, LLVMCreateStringAttribute, LLVMGetBasicBlockParent, LLVMGetGlobalParent,
  LLVMGetInstructionParent, LLVMGetModuleContext, LLVMRemoveCallSiteStringAttribute,
};
use llvm_sys::prelude::{LLVMContextRef, LLVMValueRef};
use std::ffi::{CStr, CString};

#[cfg(feature = "statepoint-directives")]
use llvm_sys::core::LLVMGetCallSiteStringAttribute;

fn instruction_context(call: LLVMValueRef) -> LLVMContextRef {
  unsafe {
    let bb = LLVMGetInstructionParent(call);
    assert!(!bb.is_null(), "callsite has no parent basic block");

    let func = LLVMGetBasicBlockParent(bb);
    assert!(!func.is_null(), "callsite has no parent function");

    let module = LLVMGetGlobalParent(func);
    assert!(!module.is_null(), "callsite has no parent module");

    LLVMGetModuleContext(module)
  }
}

fn add_callsite_string_attr(call: LLVMValueRef, key: &CStr, value: &CStr) {
  let ctx = instruction_context(call);

  unsafe {
    // Ensure the key is unique. LLVM treats string attributes as a key/value mapping, but the C API
    // is "add-only"; remove any existing entry first so we can reliably update/override values.
    LLVMRemoveCallSiteStringAttribute(
      call,
      llvm_sys::LLVMAttributeFunctionIndex,
      key.as_ptr(),
      key.to_bytes().len() as u32,
    );

    let attr = LLVMCreateStringAttribute(
      ctx,
      key.as_ptr(),
      key.to_bytes().len() as u32,
      value.as_ptr(),
      value.to_bytes().len() as u32,
    );

    // "Callsite" attributes live at the function attribute index.
    LLVMAddCallSiteAttribute(call, llvm_sys::LLVMAttributeFunctionIndex, attr);
  }
}

#[cfg(feature = "statepoint-directives")]
fn has_callsite_string_attr(call: LLVMValueRef, key: &CStr) -> bool {
  unsafe {
    !LLVMGetCallSiteStringAttribute(
      call,
      llvm_sys::LLVMAttributeFunctionIndex,
      key.as_ptr(),
      key.to_bytes().len() as u32,
    )
    .is_null()
  }
}

/// Attach LLVM 18 statepoint directive `"statepoint-id"="<u64>"` to a `call`/`invoke` instruction.
///
/// This must be set on the *original* callsite before running LLVM's
/// `RewriteStatepointsForGC` / `rewrite-statepoints-for-gc` pass.
///
/// The ID becomes the StackMap record's patchpoint ID.
pub fn set_callsite_statepoint_id(call: LLVMValueRef, id: u64) {
  let key = CString::new("statepoint-id").expect("statepoint-id must not contain NULs");
  let value = CString::new(id.to_string()).expect("statepoint-id must not contain NULs");
  add_callsite_string_attr(call, &key, &value);
}

/// Attach LLVM 18 statepoint directive `"statepoint-num-patch-bytes"="<u32>"` to a `call`/`invoke`
/// instruction.
///
/// This must be set on the *original* callsite before running LLVM's
/// `RewriteStatepointsForGC` / `rewrite-statepoints-for-gc` pass.
///
/// - `bytes = 0`: LLVM emits a normal `call` instruction.
/// - `bytes > 0`: LLVM reserves a patchable region at the callsite (x86_64: a NOP sled) and emits
///   a stackmap record keyed by the *end* of that reserved region (the return address if/when a
///   call is patched in).
pub fn set_callsite_statepoint_num_patch_bytes(call: LLVMValueRef, bytes: u32) {
  let key = CString::new("statepoint-num-patch-bytes")
    .expect("statepoint-num-patch-bytes key must not contain NULs");
  let value =
    CString::new(bytes.to_string()).expect("statepoint-num-patch-bytes must not contain NULs");
  add_callsite_string_attr(call, &key, &value);
}

/// Walk all call/invoke instructions in GC-managed functions and assign sequential `"statepoint-id"`
/// values.
///
/// This is an optional helper intended to run before LLVM's `rewrite-statepoints-for-gc` pass so
/// the resulting `gc.statepoint` IDs are deterministic and/or unique.
#[cfg(feature = "statepoint-directives")]
pub fn assign_statepoint_ids(
  module: llvm_sys::prelude::LLVMModuleRef,
  start: u64,
) -> anyhow::Result<()> {
  use llvm_sys::core::{
    LLVMGetFirstBasicBlock, LLVMGetFirstFunction, LLVMGetFirstInstruction, LLVMGetGC,
    LLVMGetInstructionOpcode, LLVMGetNextBasicBlock, LLVMGetNextFunction, LLVMGetNextInstruction,
  };
  use llvm_sys::LLVMOpcode;

  let mut next_id = start;
  let statepoint_id_key = CString::new("statepoint-id").expect("statepoint-id must not contain NULs");

  unsafe {
    let mut func = LLVMGetFirstFunction(module);
    while !func.is_null() {
      // Only GC-managed functions are rewritten into statepoints.
      if !LLVMGetGC(func).is_null() {
        let mut bb = LLVMGetFirstBasicBlock(func);
        while !bb.is_null() {
          let mut inst = LLVMGetFirstInstruction(bb);
          while !inst.is_null() {
            match LLVMGetInstructionOpcode(inst) {
              LLVMOpcode::LLVMCall | LLVMOpcode::LLVMInvoke => {
                // Preserve explicitly-assigned IDs.
                if !has_callsite_string_attr(inst, &statepoint_id_key) {
                  set_callsite_statepoint_id(inst, next_id);
                  next_id += 1;
                }
              }
              _ => {}
            }
            inst = LLVMGetNextInstruction(inst);
          }
          bb = LLVMGetNextBasicBlock(bb);
        }
      }

      func = LLVMGetNextFunction(func);
    }
  }

  Ok(())
}
