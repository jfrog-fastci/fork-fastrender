use inkwell::context::Context;
use inkwell::types::PointerType;
use inkwell::values::{AsValueRef, FunctionValue};
use inkwell::AddressSpace;
use llvm_sys::core::LLVMSetGC;
use std::ffi::CString;

/// The LLVM GC strategy name used by `native-js`.
///
/// LLVM selects statepoint lowering rules based on the function-level
/// `gc "<strategy>"` attribute. `native-js` standardizes on a single strategy name
/// to avoid drift between modules and to reduce the chance of future LLVM
/// breakage.
///
/// See `native-js/docs/llvm_gc_strategy.md` for rationale.
pub const GC_STRATEGY: &str = "coreclr";

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

/// Mark a function as GC-managed, e.g. `gc "coreclr"`.
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
pub fn set_default_gc_strategy(function: &FunctionValue<'_>) -> Result<(), std::ffi::NulError> {
  set_gc_strategy(function, GC_STRATEGY)
}

/// Backwards-compatibility alias for older tests/examples.
#[deprecated(note = "native-js no longer uses `gc \\\"statepoint-example\\\"`; use `set_default_gc_strategy` instead")]
#[inline]
pub fn set_statepoint_example_gc(function: &FunctionValue<'_>) -> Result<(), std::ffi::NulError> {
  set_default_gc_strategy(function)
}
