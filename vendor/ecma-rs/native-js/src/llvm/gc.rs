use inkwell::context::Context;
use inkwell::types::PointerType;
use inkwell::values::{AsValueRef, FunctionValue};
use inkwell::AddressSpace;
use llvm_sys::core::LLVMSetGC;
use std::ffi::CString;

/// The GC strategy name used by LLVM's built-in statepoint example collector.
pub const GC_STRATEGY: &str = "statepoint-example";

/// Address space used for GC-managed pointers.
///
/// LLVM's statepoint lowering identifies GC references by address space, so
/// native-js models all GC-managed pointers as `ptr addrspace(1)`.
pub const GC_ADDR_SPACE: u32 = 1;

#[inline]
pub fn gc_address_space() -> AddressSpace {
  // Inkwell's AddressSpace is `u16` (matches LLVM C API).
  AddressSpace::from(GC_ADDR_SPACE as u16)
}

/// A `ptr addrspace(1)` type for representing GC-managed pointers.
#[inline]
pub fn gc_ptr_type<'ctx>(ctx: &'ctx Context) -> PointerType<'ctx> {
  ctx.ptr_type(gc_address_space())
}

/// Mark a function as GC-managed, e.g. `gc "statepoint-example"`.
///
/// This is required for `rewrite-statepoints-for-gc` to rewrite calls in the
/// function into statepoints.
pub fn set_gc_strategy(function: &FunctionValue<'_>, strategy: &str) -> Result<(), std::ffi::NulError> {
  let strategy = CString::new(strategy)?;
  unsafe {
    LLVMSetGC(function.as_value_ref(), strategy.as_ptr());
  }
  Ok(())
}

/// Convenience wrapper for [`GC_STRATEGY`].
#[inline]
pub fn set_statepoint_example_gc(function: &FunctionValue<'_>) -> Result<(), std::ffi::NulError> {
  set_gc_strategy(function, GC_STRATEGY)
}
