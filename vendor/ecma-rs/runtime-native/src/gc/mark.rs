use std::alloc::handle_alloc_error;
use std::alloc::Layout;
use std::collections::VecDeque;
use std::mem;
use std::ptr;
use std::time::Instant;

use ahash::AHashSet;

use super::roots::RememberedSet;
use super::roots::RootSet;
use super::weak::process_global_weak_handles_major;
use super::weak::run_weak_cleanups;
use super::ObjHeader;
use super::Tracer;
use crate::gc::heap::GcHeap;
use crate::gc::heap::IMMIX_LINES_PER_BLOCK;
use crate::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use crate::immix::BumpCursor;

impl GcHeap {
  /// Perform a full-heap major collection using a mark-region algorithm over
  /// Immix blocks plus sweeping of the large-object space.
  ///
  /// This method begins with a [`GcHeap::collect_minor`] to ensure no nursery
  /// objects remain.
  ///
  /// # Stop-the-world requirement
  /// This GC is **stop-the-world**: the caller must ensure there are no
  /// concurrent mutators and that the provided root/remembered sets remain
  /// stable for the duration of the call.
  pub fn collect_major(&mut self, roots: &mut dyn RootSet, remembered: &mut dyn RememberedSet) {
    self.collect_minor(roots, remembered);
    self.stats.major_collections += 1;
    let start = Instant::now();

    // Toggle the epoch so we can treat previous marks as "unmarked" without
    // clearing every object header.
    self.mark_epoch ^= 1;
    let epoch = self.mark_epoch;

    // Reset all Immix liveness maps.
    self.immix.clear_line_marks();

    {
      let mut marker = Marker {
        heap: self,
        epoch,
        worklist: Vec::new(),
      };

      roots.for_each_root_slot(&mut |slot| {
        marker.visit_slot(slot);
      });

      let mut root_handles = mem::take(&mut marker.heap.root_handles);
      root_handles.for_each_root_slot(&mut |slot| {
        marker.visit_slot(slot);
      });
      marker.heap.root_handles = root_handles;

      while let Some(obj) = marker.worklist.pop() {
        marker.visit_obj(obj);
      }
    }

    let mut candidate_blocks = vec![false; self.immix.block_count()];
    let mut candidate_count = 0usize;
    let cfg = self.major_compaction;
    if cfg.enabled && cfg.max_live_ratio_percent <= 100 {
      for block_id in 0..self.immix.block_count() {
        let Some(metrics) = self.immix.block_metrics(block_id) else {
          continue;
        };

        let live_lines = IMMIX_LINES_PER_BLOCK - metrics.free_lines;
        if live_lines == 0 {
          continue;
        }
        if live_lines < cfg.min_live_lines {
          continue;
        }

        if live_lines * 100 < cfg.max_live_ratio_percent as usize * IMMIX_LINES_PER_BLOCK {
          candidate_blocks[block_id] = true;
          candidate_count += 1;
        }
      }
    }

    if candidate_count > 0 {
      {
        let mut compactor = Compactor {
          heap: self,
          candidate_blocks: &candidate_blocks,
          worklist: VecDeque::new(),
          visited: AHashSet::new(),
          bump: BumpCursor::new(),
        };

        roots.for_each_root_slot(&mut |slot| {
          compactor.visit_slot(slot);
        });

        let mut root_handles = mem::take(&mut compactor.heap.root_handles);
        root_handles.for_each_root_slot(&mut |slot| {
          compactor.visit_slot(slot);
        });
        compactor.heap.root_handles = root_handles;

        while let Some(obj) = compactor.worklist.pop_front() {
          compactor.visit_obj(obj);
        }
      }

      for (block_id, is_candidate) in candidate_blocks.iter().enumerate() {
        if *is_candidate {
          self.immix.clear_block_line_map(block_id);
        }
      }
    }

    self.process_weak_handles_major(epoch);
    process_global_weak_handles_major(self, epoch);
    run_weak_cleanups(self);
    self.stats.last_major_live_bytes = self.immix.line_map_used_bytes() + self.los.live_bytes(epoch);
    self.immix.finalize_after_marking();
    self.los.sweep(epoch);

    let pause = start.elapsed();
    self.stats.last_major_pause = pause;
    self.stats.total_major_pause += pause;
  }
}

struct Marker<'a> {
  heap: &'a mut GcHeap,
  epoch: u8,
  worklist: Vec<*mut u8>,
}

impl Marker<'_> {
  fn mark_obj(&mut self, mut obj: *mut u8) {
    if obj.is_null() {
      return;
    }

    debug_assert!(
      !self.heap.is_in_nursery(obj),
      "major GC must not see nursery pointers (minor GC should run first)"
    );

    // SAFETY: `obj` is expected to be a valid heap object.
    unsafe {
      let header = &*(obj as *const ObjHeader);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
      }
    }

    // SAFETY: `obj` points to an `ObjHeader`.
    let already_marked = unsafe { (&*(obj as *const ObjHeader)).is_marked(self.epoch) };
    if already_marked {
      return;
    }

    // SAFETY: `obj` points to an `ObjHeader`.
    unsafe {
      let header = &mut *(obj as *mut ObjHeader);
      header.set_mark_epoch(self.epoch);

      let size = super::obj_size(obj);
      if self.heap.is_in_immix(obj) {
        self.heap.immix.set_lines_for_live_object(obj, size);
      } else {
        debug_assert!(self.heap.is_in_los(obj), "unknown heap object location");
      }
    }

    self.worklist.push(obj);
  }
}

impl Tracer for Marker<'_> {
  fn visit_slot(&mut self, slot: *mut *mut u8) {
    // SAFETY: `slot` originates from root enumeration or from a valid object
    // descriptor, so it is a valid pointer to a GC reference.
    let obj = unsafe { *slot };
    self.mark_obj(obj);
  }
}

struct Compactor<'a> {
  heap: &'a mut GcHeap,
  candidate_blocks: &'a [bool],
  worklist: VecDeque<*mut u8>,
  visited: AHashSet<usize>,
  bump: BumpCursor,
}

impl Compactor<'_> {
  fn enqueue_obj(&mut self, obj: *mut u8) {
    if obj.is_null() {
      return;
    }
    if !self.visited.insert(obj as usize) {
      return;
    }
    self.worklist.push_back(obj);
  }

  fn is_candidate_obj(&self, obj: *mut u8) -> bool {
    if !self.heap.is_in_immix(obj) {
      return false;
    }
    let Some(block_id) = self.heap.immix.block_id_for_ptr(obj) else {
      return false;
    };
    self.candidate_blocks.get(block_id).copied().unwrap_or(false)
  }

  fn alloc_to_space(&mut self, size: usize, align: usize) -> *mut u8 {
    let obj = if size > IMMIX_MAX_OBJECT_SIZE {
      self.heap.los.alloc(size, align)
    } else {
      self
        .heap
        .immix
        .alloc_old_with_cursor_excluding(&mut self.bump, size, align, self.candidate_blocks)
        .unwrap_or_else(|| handle_alloc_error(Layout::from_size_align(size, align).unwrap()))
    };

    self.heap.stats.bytes_allocated_old += size;
    obj
  }

  fn evacuate(&mut self, obj: *mut u8) -> *mut u8 {
    debug_assert!(self.is_candidate_obj(obj));

    // SAFETY: `obj` is expected to be a valid heap object.
    unsafe {
      let header = &mut *(obj as *mut ObjHeader);
      if header.is_forwarded() {
        return header.forwarding_ptr();
      }
      if header.is_pinned() {
        return obj;
      }

      let size = super::obj_size(obj);

      let new_obj = self.alloc_to_space(size, mem::align_of::<ObjHeader>());
      ptr::copy_nonoverlapping(obj, new_obj, size);
      header.set_forwarding_ptr(new_obj);
      new_obj
    }
  }
}

impl Tracer for Compactor<'_> {
  fn visit_slot(&mut self, slot: *mut *mut u8) {
    // SAFETY: `slot` originates from root enumeration or from a valid object
    // descriptor, so it is a valid pointer to a GC reference.
    let mut obj = unsafe { *slot };
    if obj.is_null() {
      return;
    }

    debug_assert!(
      !self.heap.is_in_nursery(obj),
      "major GC must not see nursery pointers (minor GC should run first)"
    );

    // Follow forwarding pointers (objects already evacuated from candidate
    // blocks) and update the slot.
    // SAFETY: `obj` is expected to be a valid heap object.
    unsafe {
      let header = &*(obj as *const ObjHeader);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
        *slot = obj;
      }
    }

    if self.is_candidate_obj(obj) {
      let new_obj = self.evacuate(obj);
      if new_obj != obj {
        // SAFETY: `slot` is valid and writable.
        unsafe {
          *slot = new_obj;
        }
        obj = new_obj;
      }
    }

    self.enqueue_obj(obj);
  }
}
