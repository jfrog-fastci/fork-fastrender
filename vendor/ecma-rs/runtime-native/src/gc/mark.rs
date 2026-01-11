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
use crate::gc::heap::AllocError;
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
  pub fn collect_major(
    &mut self,
    roots: &mut dyn RootSet,
    remembered: &mut dyn RememberedSet,
  ) -> Result<(), AllocError> {
    let _gc_guard = super::GcInProgressGuard::new();
    self.collect_minor(roots, remembered)?;
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
      };
      marker.heap.work_stack.clear();

      roots.for_each_root_slot(&mut |slot| {
        marker.visit_slot(slot);
      });

      // Process-global roots/handles registered outside of stackmaps (intern tables, runtime-owned
      // queues, host handles, ...).
      crate::roots::global_root_registry().for_each_root_slot(|slot| marker.visit_slot(slot));
      crate::roots::global_persistent_handle_table().for_each_root_slot(|slot| marker.visit_slot(slot));

      let mut root_handles = mem::take(&mut marker.heap.root_handles);
      root_handles.for_each_root_slot(&mut |slot| {
        marker.visit_slot(slot);
      });
      marker.heap.root_handles = root_handles;

      while let Some(obj) = marker.heap.work_stack.pop() {
        marker.visit_obj(obj);
      }
    }

    let cfg = self.major_compaction;
    if cfg.enabled && cfg.max_live_ratio_percent <= 100 {
      // Major compaction is optional and disabled by default. Avoid allocating the candidate bitmap
      // unless we actually find candidate blocks.
      let candidate_blocks_opt = {
        let immix = &self.immix;

        let is_candidate_block = |block_id: usize| -> bool {
          let Some(metrics) = immix.block_metrics(block_id) else {
            return false;
          };

          let live_lines = IMMIX_LINES_PER_BLOCK - metrics.free_lines;
          if live_lines == 0 {
            return false;
          }
          if live_lines < cfg.min_live_lines {
            return false;
          }

          live_lines * 100 < cfg.max_live_ratio_percent as usize * IMMIX_LINES_PER_BLOCK
        };

        let mut candidate_count = 0usize;
        for block_id in 0..immix.block_count() {
          if is_candidate_block(block_id) {
            candidate_count += 1;
          }
        }

        if candidate_count == 0 {
          None
        } else {
          let mut candidate_blocks = vec![false; immix.block_count()];
          for block_id in 0..candidate_blocks.len() {
            if is_candidate_block(block_id) {
              candidate_blocks[block_id] = true;
            }
          }
          Some(candidate_blocks)
        }
      };

      if let Some(candidate_blocks) = candidate_blocks_opt {
        // Rebuild the Immix availability structure so evacuation can allocate
        // into existing holes (opportunistic copying) instead of always growing
        // the heap with new blocks.
        //
        // We will rebuild again after compaction since clearing candidate blocks
        // and allocating new objects changes hole sizes.
        self.immix.finalize_after_marking();

        let mut pinned_in_candidates: Vec<Vec<*mut u8>> = vec![Vec::new(); candidate_blocks.len()];

        {
          let mut compactor = Compactor {
            heap: self,
            candidate_blocks: &candidate_blocks,
            pinned_in_candidates: &mut pinned_in_candidates,
            worklist: VecDeque::new(),
            visited: AHashSet::new(),
            bump: BumpCursor::new(),
          };

          roots.for_each_root_slot(&mut |slot| {
            compactor.visit_slot(slot);
          });

          crate::roots::global_root_registry().for_each_root_slot(|slot| compactor.visit_slot(slot));
          crate::roots::global_persistent_handle_table()
            .for_each_root_slot(|slot| compactor.visit_slot(slot));

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
            // If we encountered pinned objects in a candidate block, we cannot
            // evacuate them. Re-mark their lines so they remain live and the
            // block is not treated as fully free.
            for &pinned in &pinned_in_candidates[block_id] {
              unsafe {
                let size = super::obj_size(pinned);
                self.immix.set_lines_for_live_object(pinned, size);
              }
            }
          }
        }
      }
    }

    self.process_weak_handles_major(epoch);
    process_global_weak_handles_major(self, epoch);
    run_weak_cleanups(self);
    self.process_finalizers_major(epoch);
    self.stats.last_major_live_bytes = self.immix.line_map_used_bytes() + self.los.live_bytes(epoch);
    self.immix.finalize_after_marking();
    self.los.sweep(epoch);

    let pause = start.elapsed();
    self.stats.last_major_pause = pause;
    self.stats.total_major_pause += pause;
    Ok(())
  }
}

struct Marker<'a> {
  heap: &'a mut GcHeap,
  epoch: u8,
}

impl Marker<'_> {
  fn mark_obj(&mut self, mut obj: *mut u8) {
    if obj.is_null() {
      return;
    }

    // `collect_major` runs `collect_minor` first, so in the common case there should be no nursery
    // pointers left. Handle them (and any stale/foreign pointers) defensively anyway.
    loop {
      if !self.heap.is_valid_obj_ptr_for_tracing(obj, true) {
        return;
      }

      if self.heap.is_in_nursery(obj) {
        // SAFETY: `obj` is a valid pointer into this heap's nursery.
        unsafe {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            obj = header.forwarding_ptr();
            continue;
          }
        }
        return;
      }

      // Follow forwarding pointers (used by nursery evacuation today, and by potential future major
      // GC compaction).
      // SAFETY: `obj` is in this heap (Immix or LOS), so it points at an `ObjHeader`.
      unsafe {
        let header = &*(obj as *const ObjHeader);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
          continue;
        }
      }

      break;
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
    self.heap.work_stack.push(obj);
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
  pinned_in_candidates: &'a mut [Vec<*mut u8>],
  worklist: VecDeque<*mut u8>,
  visited: AHashSet<usize>,
  bump: BumpCursor,
}

impl Compactor<'_> {
  fn enqueue_obj(&mut self, obj: *mut u8) -> bool {
    if obj.is_null() {
      return false;
    }
    if !self.visited.insert(obj as usize) {
      return false;
    }
    self.worklist.push_back(obj);
    true
  }

  fn candidate_block_id(&self, obj: *mut u8) -> Option<usize> {
    if !self.heap.is_in_immix(obj) {
      return None;
    }
    let Some(block_id) = self.heap.immix.block_id_for_ptr(obj) else {
      return None;
    };
    if self.candidate_blocks.get(block_id).copied().unwrap_or(false) {
      Some(block_id)
    } else {
      None
    }
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
    debug_assert!(self.candidate_block_id(obj).is_some());

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

    // `collect_major` runs `collect_minor` first, so in the common case there should be no nursery
    // pointers left. Handle them (and any stale/foreign pointers) defensively anyway.
    loop {
      if !self.heap.is_valid_obj_ptr_for_tracing(obj, true) {
        return;
      }

      if self.heap.is_in_nursery(obj) {
        // SAFETY: `obj` is a valid pointer into this heap's nursery.
        unsafe {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            obj = header.forwarding_ptr();
            // SAFETY: `slot` is valid and writable.
            *slot = obj;
            continue;
          }
        }
        return;
      }

      // Follow forwarding pointers (objects already evacuated from candidate blocks) and update the
      // slot.
      // SAFETY: `obj` is in this heap (Immix or LOS), so it points at an `ObjHeader`.
      unsafe {
        let header = &*(obj as *const ObjHeader);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
          // SAFETY: `slot` is valid and writable.
          *slot = obj;
          continue;
        }
      }

      break;
    }

    let mut pinned_block_id: Option<usize> = None;
    if let Some(block_id) = self.candidate_block_id(obj) {
      // Pinned objects must remain in place; remember them so we can re-mark
      // their lines after clearing the candidate block.
      // SAFETY: `obj` is expected to be a valid heap object.
      if unsafe { (&*(obj as *const ObjHeader)).is_pinned() } {
        pinned_block_id = Some(block_id);
      } else {
        let new_obj = self.evacuate(obj);
        if new_obj != obj {
          // SAFETY: `slot` is valid and writable.
          unsafe {
            *slot = new_obj;
          }
          obj = new_obj;
        }
      }
    }

    let first_visit = self.enqueue_obj(obj);
    if first_visit {
      if let Some(block_id) = pinned_block_id {
        self.pinned_in_candidates[block_id].push(obj);
      }
    }
  }
}
