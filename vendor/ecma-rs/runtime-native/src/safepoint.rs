use crate::arch;
use crate::stackmaps::StackMaps;
use crate::threading;
use crate::WalkError;
use std::sync::OnceLock;
use std::time::Duration;

/// Lazily parse the in-memory `.llvm_stackmaps` section, if available.
///
/// Many unit/integration tests do not link a real stackmap section, so this
/// returns `None` when the section is missing.
fn global_stackmaps() -> Option<&'static StackMaps> {
  static STACKMAPS: OnceLock<Option<StackMaps>> = OnceLock::new();
  STACKMAPS
    .get_or_init(|| {
      let bytes = crate::stackmaps_loader::stackmaps_section();
      if bytes.is_empty() {
        return None;
      }
      Some(StackMaps::parse(bytes).expect("failed to parse .llvm_stackmaps section"))
    })
    .as_ref()
}

/// Visit `(slot, value)` relocation pairs for GC roots reachable from a safepoint.
///
/// `top_callee_fp` must be the frame pointer of the runtime entrypoint frame
/// (`rt_gc_safepoint` / `rt_gc_collect`) at the safepoint callsite.
pub fn visit_reloc_pairs(
  top_callee_fp: u64,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  let Some(stackmaps) = global_stackmaps() else {
    return Ok(());
  };

  // Safety: stackmap-driven root enumeration inherently walks raw pointers into
  // thread stacks.
  unsafe {
    crate::walk_gc_roots_from_fp(top_callee_fp, stackmaps, |slot_addr| {
      let slot = slot_addr as *mut *mut u8;
      // Read the current pointer value in the slot so GC can relocate it.
      let value = slot.read();
      visit(slot, value);
    })
  }
}

/// Cooperatively enter a safepoint at the current callsite while a stop-the-world
/// request is active.
///
/// This is used by `rt_gc_collect` when a thread attempts to initiate GC while a
/// collection is already in progress: it must still publish a safepoint context
/// and block until the world is resumed.
pub(crate) fn enter_safepoint_at_current_callsite(stop_epoch: u64) {
  let ctx = arch::capture_safepoint_context();
  threading::registry::set_current_thread_safepoint_context(ctx);
  threading::registry::set_current_thread_safepoint_epoch_observed(stop_epoch);
  threading::safepoint::notify_state_change();

  // Block until the stop-the-world epoch is resumed.
  threading::safepoint::wait_while_stop_the_world();
}

pub(crate) fn with_world_stopped_requested(stop_epoch: u64, f: impl FnOnce()) {
  struct ResumeOnDrop;
  impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
      let _ = crate::rt_gc_resume_world();
    }
  }
  let _resume = ResumeOnDrop;

  let timeout = Duration::from_secs(2);
  assert!(
    crate::rt_gc_wait_for_world_stopped_timeout(timeout),
    "world did not reach safepoint in time (timeout={timeout:?}, epoch={stop_epoch})"
  );

  // Root enumeration hook. This is intentionally a "plumbing" step: we count
  // roots in debug builds to validate that per-thread context capture and
  // stackmap lookup don't crash.
  for thread in threading::all_threads() {
    if thread.is_parked() || thread.is_native_safe() {
      continue;
    }
    if thread.safepoint_epoch_observed() != stop_epoch {
      continue;
    }

    let Some(ctx) = thread.safepoint_context() else {
      continue;
    };

    let mut roots = 0usize;
    let _ = visit_reloc_pairs(ctx.fp as u64, &mut |_, _| roots += 1);
    let _ = (ctx, roots);
  }

  f();
}

/// Stop the world, enumerate roots, run `f`, and resume.
///
/// This is a coordinator-side API; mutator threads should normally stop via
/// `rt_gc_safepoint`.
pub fn with_world_stopped(f: impl FnOnce()) {
  let stop_epoch = crate::rt_gc_request_stop_the_world();
  with_world_stopped_requested(stop_epoch, f);
}
