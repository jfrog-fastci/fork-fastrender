/// A precise root set enumerator.
///
/// The slots passed to the callback are mutable pointers to GC references
/// (`*mut u8`). A GC implementation may update the slot in-place (e.g. when
/// evacuating a young object).
pub trait RootSet {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8));
}

/// Simple root set implementation used by unit tests.
#[derive(Default)]
pub struct RootStack {
  slots: Vec<*mut *mut u8>,
}

impl RootStack {
  pub fn new() -> Self {
    Self { slots: Vec::new() }
  }

  pub fn push(&mut self, slot: *mut *mut u8) {
    self.slots.push(slot);
  }
}

impl RootSet for RootStack {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    for &slot in &self.slots {
      f(slot);
    }
  }
}

/// Old-to-young remembered set, parameterized by write-barrier policy.
pub trait RememberedSet {
  /// Enumerate old-generation objects that may contain pointers into the nursery.
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8));

  /// Clear all remembered entries.
  fn clear(&mut self);
}

