/// Forces the compiler to treat `gc_ref` as live until this call site.
///
/// This is the native equivalent of Go's `runtime.KeepAlive`: it is used when optimized/native
/// code derives a raw (non-GC) pointer from a GC-managed object and must ensure the owning GC
/// object cannot be collected/finalized before the last raw-pointer use.
///
/// `gc_ref` is a pointer to a GC-managed object header (the same base pointer returned from
/// `rt_alloc`). It is **not** a backing-store pointer.
///
/// The implementation is intentionally opaque to LLVM:
/// - `#[inline(never)]` prevents inlining (helps preserve the keep-alive point).
/// - The inline asm is treated as having side effects, so the call cannot be DCE'd or moved
///   arbitrarily.
#[inline(never)]
pub fn keep_alive_gc_ref(gc_ref: crate::roots::GcPtr) {
  // SAFETY: The asm block does not dereference `gc_ref`; it only makes the value observable to the
  // optimizer. We intentionally omit `nomem` so LLVM must assume this could touch memory, which
  // prevents motion across memory operations/statepoints.
  unsafe {
    core::arch::asm!(
      "/* {0} */",
      in(reg) gc_ref,
      options(nostack, preserves_flags)
    );
  }
}

/// Exported runtime ABI entrypoint used by generated code.
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_keep_alive_gc_ref(gc_ref: crate::roots::GcPtr) {
  keep_alive_gc_ref(gc_ref);
}
