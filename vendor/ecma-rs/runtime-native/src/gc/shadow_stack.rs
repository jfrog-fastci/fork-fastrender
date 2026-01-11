//! Per-thread shadow stack roots for runtime-native Rust code.
//!
//! Runtime-native mutator code (event loop, scheduler, I/O drivers) is not compiled with LLVM GC
//! statepoints, so it does not have stack maps that allow the GC to find and update live
//! references held in Rust stack frames.
//!
//! This module provides a small *shadow root stack* abstraction intended to live in per-thread
//! runtime state. Callers can explicitly push any GC pointers they need to remain valid across a
//! safepoint/allocation that may trigger GC.
//!
//! # Invariants
//! - Runtime-native code must push any [`GcRawPtr`] that will be used after a safepoint/allocation
//!   that may perform GC.
//! - [`ShadowStack::visit_roots_mut`] is intended to be called only during a stop-the-world (STW)
//!   phase, where no mutator thread is concurrently mutating its shadow stack.
//!
//! The GC may update roots in place during compaction/evacuation.
//!
//! Note: this structure intentionally stores the pointer values (not addresses of stack locals) so
//! it can be updated even when Rust frames are opaque to the GC.

use std::collections::TryReserveError;
use std::ptr::NonNull;

/// Opaque raw pointer type stored in the shadow stack.
///
/// This is intentionally untyped; higher-level code can wrap it in `GcPtr<T>` or similar.
pub type GcRawPtr = NonNull<u8>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtError {
  /// Failed to allocate memory to grow the shadow stack.
  ///
  /// Note: `Vec::try_reserve` can also fail due to capacity overflow. We currently surface both
  /// cases as `OutOfMemory` because `TryReserveErrorKind` is unstable.
  OutOfMemory,
}

impl From<TryReserveError> for RtError {
  fn from(_err: TryReserveError) -> Self {
    RtError::OutOfMemory
  }
}

impl std::fmt::Display for RtError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RtError::OutOfMemory => write!(f, "out of memory"),
    }
  }
}

impl std::error::Error for RtError {}

/// Per-thread stack of GC roots for runtime-native Rust code.
#[derive(Debug, Default)]
pub struct ShadowStack {
  slots: Vec<GcRawPtr>,
}

impl ShadowStack {
  pub fn new() -> Self {
    Self { slots: Vec::new() }
  }

  /// Create an RAII scope that truncates the shadow stack back to its entry length on drop.
  #[must_use]
  pub fn scope(&mut self) -> RootScope<'_> {
    RootScope {
      len_at_entry: self.slots.len(),
      stack: self,
    }
  }

  /// Visit each root slot mutably.
  ///
  /// The visitor may update the slot in place (e.g. during relocation/compaction).
  ///
  /// This should only be called during stop-the-world GC.
  pub fn visit_roots_mut(&mut self, mut f: impl FnMut(&mut GcRawPtr)) {
    for slot in &mut self.slots {
      f(slot);
    }
  }
}

/// RAII scope for managing a stack discipline on top of a [`ShadowStack`].
pub struct RootScope<'a> {
  stack: &'a mut ShadowStack,
  len_at_entry: usize,
}

impl RootScope<'_> {
  /// Push a single GC root onto the shadow stack.
  pub fn push_root(&mut self, ptr: GcRawPtr) -> Result<(), RtError> {
    self.stack.slots.try_reserve(1)?;
    self.stack.slots.push(ptr);
    Ok(())
  }

  /// Push multiple GC roots onto the shadow stack.
  pub fn push_roots(&mut self, ptrs: &[GcRawPtr]) -> Result<(), RtError> {
    if ptrs.is_empty() {
      return Ok(());
    }
    self.stack.slots.try_reserve(ptrs.len())?;
    self.stack.slots.extend_from_slice(ptrs);
    Ok(())
  }
}

impl Drop for RootScope<'_> {
  fn drop(&mut self) {
    self.stack.slots.truncate(self.len_at_entry);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn raw(addr: usize) -> GcRawPtr {
    assert_ne!(addr, 0);
    // The shadow stack stores opaque pointers; tests never dereference them.
    unsafe { NonNull::new_unchecked(addr as *mut u8) }
  }

  #[test]
  fn scope_truncation() {
    let mut stack = ShadowStack::new();
    assert!(stack.slots.is_empty());

    let p1 = raw(0x1000);
    let p2 = raw(0x2000);
    let p3 = raw(0x3000);

    {
      let mut outer = stack.scope();
      outer.push_root(p1).unwrap();
      outer.push_root(p2).unwrap();
      assert_eq!(outer.stack.slots.len(), 2);

      {
        let mut inner = outer.stack.scope();
        inner.push_root(p3).unwrap();
        assert_eq!(inner.stack.slots.len(), 3);
      }

      assert_eq!(outer.stack.slots.len(), 2);
    }

    assert!(stack.slots.is_empty());
  }

  #[test]
  fn push_multiple_roots() {
    let mut stack = ShadowStack::new();

    let p1 = raw(0x1111);
    let p2 = raw(0x2222);
    let p3 = raw(0x3333);

    {
      let mut scope = stack.scope();
      scope.push_roots(&[p1, p2, p3]).unwrap();
      assert_eq!(scope.stack.slots, vec![p1, p2, p3]);
    }
  }

  #[test]
  fn visit_roots_mut_relocates() {
    let mut stack = ShadowStack::new();

    let p1 = raw(0x1000);
    let p2 = raw(0x2000);
    let delta = 0x10usize;

    {
      let mut scope = stack.scope();
      scope.push_roots(&[p1, p2]).unwrap();

      scope.stack.visit_roots_mut(|slot| {
        let addr = slot.as_ptr() as usize;
        *slot = raw(addr + delta);
      });

      assert_eq!(scope.stack.slots[0].as_ptr() as usize, 0x1000 + delta);
      assert_eq!(scope.stack.slots[1].as_ptr() as usize, 0x2000 + delta);
    }
  }
}
