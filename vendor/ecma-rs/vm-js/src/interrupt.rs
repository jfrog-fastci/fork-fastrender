use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// A token observed by the VM to detect host interrupts.
#[derive(Debug, Clone)]
pub struct InterruptToken {
  internal: Arc<AtomicBool>,
  external: Option<Arc<AtomicBool>>,
}

impl InterruptToken {
  /// Create a new interrupt token + handle pair.
  pub fn new() -> (Self, InterruptHandle) {
    Self::from_internal_and_external_flags(Arc::new(AtomicBool::new(false)), None)
  }

  /// Create an interrupt token + handle pair that shares a host-owned internal flag.
  ///
  /// This is useful for integrating with cancellation/timeout infrastructure that already uses an
  /// `Arc<AtomicBool>` token, allowing the VM to observe the same flag without additional polling
  /// glue.
  ///
  /// Note: this flag is considered "internal" to the VM: it can be reset by [`InterruptToken::reset`]
  /// / [`InterruptHandle::reset`]. For a flag that is observed but never cleared by `vm-js`, use an
  /// external flag via [`InterruptToken::from_internal_and_external_flags`] or
  /// [`crate::VmOptions::external_interrupt_flag`].
  pub fn from_shared_flag(interrupted: Arc<AtomicBool>) -> (Self, InterruptHandle) {
    Self::from_internal_and_external_flags(interrupted, None)
  }

  pub fn from_internal_and_external_flags(
    internal: Arc<AtomicBool>,
    external: Option<Arc<AtomicBool>>,
  ) -> (Self, InterruptHandle) {
    let handle = InterruptHandle {
      internal: internal.clone(),
    };
    (Self { internal, external }, handle)
  }

  pub fn is_interrupted(&self) -> bool {
    if self.internal.load(Ordering::Relaxed) {
      return true;
    }
    match &self.external {
      Some(flag) => flag.load(Ordering::Relaxed),
      None => false,
    }
  }

  /// Clear the interrupt flag back to `false`.
  pub fn reset(&self) {
    self.internal.store(false, Ordering::Relaxed);
  }
}

/// A host handle used to request that the VM terminates execution.
#[derive(Debug, Clone)]
pub struct InterruptHandle {
  internal: Arc<AtomicBool>,
}

impl InterruptHandle {
  /// Request that the VM cooperatively terminates at the next `Vm::tick()`.
  pub fn interrupt(&self) {
    self.internal.store(true, Ordering::Relaxed);
  }

  /// Clear the interrupt flag back to `false`.
  ///
  /// This enables reusing a long-lived VM across multiple tasks without reconstructing it solely to
  /// clear an interrupt request.
  pub fn reset(&self) {
    self.internal.store(false, Ordering::Relaxed);
  }
}
