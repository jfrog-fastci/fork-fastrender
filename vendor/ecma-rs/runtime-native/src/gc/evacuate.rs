use std::mem;
use std::ptr;
use std::time::Instant;

use super::roots::RememberedSet;
use super::roots::RootSet;
use super::weak::process_global_weak_handles_minor;
use super::weak::run_weak_cleanups;
use super::cards::for_each_ptr_slot_in_dirty_cards;
use super::ObjHeader;
use super::Tracer;
use crate::gc::heap::AllocError;
use crate::gc::heap::GcHeap;

impl GcHeap {
  /// Perform a minor collection (nursery evacuation).
  ///
  /// # Stop-the-world requirement
  /// This GC is **stop-the-world**: the caller must ensure there are no
  /// concurrent mutators and that the provided root/remembered sets remain
  /// stable for the duration of the call.
  pub fn collect_minor(
    &mut self,
    roots: &mut dyn RootSet,
    remembered: &mut dyn RememberedSet,
  ) -> Result<(), AllocError> {
    if !super::gc_in_progress() {
      self.reserve_card_table_objects_for_minor_gc();
    }
    let _gc_guard = super::GcInProgressGuard::new();
    let start = Instant::now();
    self.stats.minor_collections += 1;
    self.work_stack.clear();

    // Snapshot nursery usage so we can optionally poison the previously-used
    // region after `reset` (helps surface stale nursery pointers quickly).
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    let nursery_poison_len = self.nursery.allocated_bytes();

    let err = {
      let mut evac = Evacuator {
        heap: self,
        err: None,
      };

      roots.for_each_root_slot(&mut |slot| {
        evac.visit_slot(slot);
      });

      // Process-global roots/handles registered outside of stackmaps (intern tables, runtime-owned
      // queues, host handles, ...).
      crate::roots::global_root_registry().for_each_root_slot(|slot| evac.visit_slot(slot));
      crate::roots::global_persistent_handle_table().for_each_root_slot(|slot| evac.visit_slot(slot));

      let mut root_handles = mem::take(&mut evac.heap.root_handles);
      root_handles.for_each_root_slot(&mut |slot| {
        evac.visit_slot(slot);
      });
      evac.heap.root_handles = root_handles;

      remembered.for_each_remembered_obj(&mut |obj| {
        #[cfg(feature = "gc_stats")]
        crate::gc_stats::record_remembered_object_scanned_minor();
        unsafe {
          for_each_ptr_slot_in_dirty_cards(obj, |slot| evac.visit_slot(slot));
        }
      });

      while let Some(obj) = evac.heap.work_stack.pop() {
        evac.visit_obj(obj);
        if evac.err.is_some() {
          break;
        }
      }
      evac.err
    };

    // After evacuation, the nursery is empty, so old objects cannot contain any
    // young pointers. Clear any per-object card tables on remembered objects so
    // future minors start from a clean slate.
    remembered.for_each_remembered_obj(&mut |obj| unsafe {
      super::clear_card_table_for_obj(obj);
    });

    // All nursery pointers reachable from roots/remembered objects should now be
    // forwarded to old-gen.
    if let Some(err) = err {
      let pause = start.elapsed();
      self.stats.last_minor_pause = pause;
      self.stats.total_minor_pause += pause;
      return Err(err);
    }
    self.process_finalizers_minor();
    self.process_weak_handles_minor();
    process_global_weak_handles_minor(self);
    self.nursery_tlab.clear();
    // SAFETY: `collect_minor` is documented as stop-the-world; there must be no
    // concurrent mutators or allocations when resetting the nursery.
    unsafe {
      self.nursery.reset();
    }
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    unsafe {
      ptr::write_bytes(self.nursery.start(), 0xDD, nursery_poison_len);
    }
    remembered.clear();
    run_weak_cleanups(self);

    let pause = start.elapsed();
    self.stats.last_minor_pause = pause;
    self.stats.total_minor_pause += pause;
    Ok(())
  }
}

struct Evacuator<'a> {
  heap: &'a mut GcHeap,
  err: Option<AllocError>,
}

impl Evacuator<'_> {
  fn evacuate(&mut self, obj: *mut u8) -> Result<(*mut u8, bool), AllocError> {
    debug_assert!(self.heap.is_in_nursery(obj));

    // SAFETY: `obj` is a valid GC object in the nursery.
    unsafe {
      let header = &mut *(obj as *mut ObjHeader);
      if header.is_forwarded() {
        return Ok((header.forwarding_ptr(), false));
      }

      let desc = header.type_desc();
      let size = super::obj_size(obj);
      let new_obj = self.heap.alloc_old_raw(size, desc.align)?;

      ptr::copy_nonoverlapping(obj, new_obj, size);

      // If this is a large pointer array being promoted to old-gen, ensure it
      // has a per-object card table so the exported write barrier can mark
      // dirty cards on future old→young stores.
      self
        .heap
        .maybe_install_card_table_for_array(new_obj, size);
      header.set_forwarding_ptr(new_obj);
      #[cfg(any(debug_assertions, feature = "gc_debug"))]
      {
        // Minor GC must not allocate; validate evacuation on the fly instead of building a list.
        self.heap.verify_forwarding_pairs(&[(obj, new_obj)]);
      }

      Ok((new_obj, true))
    }
  }
}

impl Tracer for Evacuator<'_> {
  fn visit_slot(&mut self, slot: *mut *mut u8) {
    if self.err.is_some() {
      return;
    }

    // SAFETY: `slot` originates from root enumeration or from a valid object
    // descriptor, so it is a valid pointer to a GC reference.
    let obj = unsafe { *slot };
    if obj.is_null() {
      return;
    }

    if !self.heap.is_valid_obj_ptr_for_tracing(obj, true) {
      return;
    }

    if self.heap.is_in_nursery(obj) {
      match self.evacuate(obj) {
        Ok((new_obj, is_new)) => {
          // SAFETY: `slot` is valid and writable.
          unsafe {
            *slot = new_obj;
          }
          if is_new {
            self.heap.work_stack.push(new_obj);
          }
        }
        Err(err) => {
          self.err = Some(err);
        }
      }
    }
  }
}
