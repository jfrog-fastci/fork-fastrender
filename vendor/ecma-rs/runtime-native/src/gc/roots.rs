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

  fn on_promoted_object(&mut self, obj: *mut u8, has_young_refs: bool);
}

#[derive(Default)]
pub struct SimpleRememberedSet {
  objs: Vec<*mut u8>,
}

impl SimpleRememberedSet {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn contains(&self, obj: *mut u8) -> bool {
    self.objs.contains(&obj)
  }

  fn add(&mut self, obj: *mut u8) {
    debug_assert!(!obj.is_null());
    // SAFETY: `obj` must point to the start of a valid GC-managed object.
    let header = unsafe { &mut *(obj as *mut super::ObjHeader) };
    if header.is_remembered() {
      return;
    }
    header.set_remembered(true);
    self.objs.push(obj);
  }

  fn remove(&mut self, obj: *mut u8) {
    if obj.is_null() {
      return;
    }
    // SAFETY: `obj` must point to the start of a valid GC-managed object.
    let header = unsafe { &mut *(obj as *mut super::ObjHeader) };
    header.set_remembered(false);

    if let Some(idx) = self.objs.iter().position(|&x| x == obj) {
      self.objs.swap_remove(idx);
    }
  }

  pub fn scan_and_rebuild(&mut self, mut object_has_young_refs: impl FnMut(*mut u8) -> bool) {
    let mut new = Vec::with_capacity(self.objs.len());
    for &obj in &self.objs {
      if object_has_young_refs(obj) {
        new.push(obj);
      } else {
        // SAFETY: `obj` must point to the start of a valid GC-managed object.
        unsafe { (&mut *(obj as *mut super::ObjHeader)).set_remembered(false) };
      }
    }
    self.objs = new;
  }
}

impl RememberedSet for SimpleRememberedSet {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    for &obj in &self.objs {
      f(obj);
    }
  }

  fn clear(&mut self) {
    for &obj in &self.objs {
      // SAFETY: `obj` must point to the start of a valid GC-managed object.
      unsafe { (&mut *(obj as *mut super::ObjHeader)).set_remembered(false) };
    }
    self.objs.clear();
  }

  fn on_promoted_object(&mut self, obj: *mut u8, has_young_refs: bool) {
    if has_young_refs {
      self.add(obj);
    } else {
      self.remove(obj);
    }
  }
}
