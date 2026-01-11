use crate::arch;
use crate::arch::SafepointContext;
use crate::stackwalk::StackBounds;
use crate::threading;
use crate::WalkError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// A starting point for stack walking.
///
/// # Codegen contract
///
/// The native code generator must keep **frame pointers enabled** (e.g.
/// `-C force-frame-pointers=yes`) so that `fp` forms a valid linked list of
/// frames.
///
/// At a GC safepoint, we must begin stack walking from the **mutator caller**
/// of `rt_gc_safepoint`, not the runtime's own frame (which has no stackmap
/// record).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct FrameCursor {
  pub fp: usize,
  pub pc: usize,
}

pub type RtGcSafepointHook = extern "C" fn(FrameCursor);

static RT_GC_SAFEPOINT_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Install a process-wide diagnostic hook invoked from `rt_gc_safepoint`'s slow
/// path.
///
/// This is intended for tests and internal debugging. It runs on the mutator
/// thread while parked in the runtime.
pub fn set_rt_gc_safepoint_hook(hook: Option<RtGcSafepointHook>) {
  let ptr = hook.map_or(0, |f| f as usize);
  RT_GC_SAFEPOINT_HOOK.store(ptr, Ordering::Release);
}

fn maybe_call_safepoint_hook(cursor: FrameCursor) {
  let ptr = RT_GC_SAFEPOINT_HOOK.load(Ordering::Acquire);
  if ptr == 0 {
    return;
  }
  // SAFETY: `set_rt_gc_safepoint_hook` only stores valid function pointers.
  let hook: RtGcSafepointHook = unsafe { std::mem::transmute(ptr) };
  hook(cursor);
}

pub fn current_thread_safepoint_cursor() -> FrameCursor {
  let Some(state) = crate::threading::registry::current_thread_state() else {
    return FrameCursor::default();
  };
  state.safepoint_cursor().unwrap_or_default()
}

#[cold]
extern "C" fn rt_gc_safepoint_slow_impl(caller_fp: usize, caller_pc: usize, sp_entry: usize) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_safepoint();

  // `rt_gc_safepoint` is only meaningful for threads that have been registered
  // with `rt_thread_init` / `rt_thread_register`. For non-attached threads this
  // is a no-op: they do not participate in stop-the-world coordination.
  if crate::threading::registry::current_thread_id().is_none() {
    return;
  }

  let cursor = FrameCursor {
    fp: caller_fp,
    pc: caller_pc,
  };

  // Also publish a `SafepointContext` so stop-the-world root enumeration can use
  // `stackwalk_fp::walk_gc_roots_from_safepoint_context`.
  // `sp` must match LLVM stackmap semantics: the caller's stack pointer value at
  // the safepoint return address.
  #[cfg(target_arch = "x86_64")]
  let sp = sp_entry.wrapping_add(arch::WORD_SIZE);
  #[cfg(target_arch = "aarch64")]
  let sp = sp_entry;
  let ctx = SafepointContext {
    sp_entry,
    sp,
    fp: caller_fp,
    ip: caller_pc,
  };
  let mut published_ctx = false;

  // Park until the coordinator resumes the world. In rare cases a thread can
  // miss a resume wakeup (the epoch can flip even->odd before this thread wakes
  // up and re-checks it). Therefore we must be prepared to observe multiple
  // stop-the-world epochs while still blocked in the runtime: publish the
  // currently requested epoch as "observed" and then wait until the epoch
  // changes, repeating until it becomes even.
  loop {
    let requested_epoch = crate::threading::safepoint::current_epoch();
    if requested_epoch & 1 == 0 {
      // Post-resume barrier: publish that we've observed the resumed epoch before
      // returning to mutator execution. Without this, the GC coordinator may
      // start a new stop-the-world cycle before this thread has returned from
      // the previous safepoint, causing it to miss the brief even epoch and
      // remain blocked across requests.
      if published_ctx {
        crate::threading::registry::set_current_thread_safepoint_epoch_observed(requested_epoch);
        crate::threading::safepoint::notify_state_change();
      }
      return;
    }

    if !published_ctx {
      crate::threading::registry::set_current_thread_safepoint_cursor(cursor);
      crate::threading::registry::set_current_thread_safepoint_context(ctx);
      published_ctx = true;
    }

    // Publish that we've observed the stop-the-world request.
    crate::threading::registry::set_current_thread_safepoint_epoch_observed(requested_epoch);
    crate::threading::safepoint::notify_state_change();

    // Test-only hook (may resume the world).
    maybe_call_safepoint_hook(cursor);

    crate::threading::safepoint::wait_while_epoch_is(requested_epoch);
  }
}

#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
#[no_mangle]
pub extern "C" fn rt_gc_safepoint() {
  core::arch::naked_asm!(
    // Capture the mutator caller cursor before any compiler-generated prologue
    // touches the stack/frame pointer:
    //   - `rbp` is the caller FP (with frame pointers enabled)
    //   - `rsp` points at the return address into the caller
    "mov rdi, rbp",
    "mov rsi, [rsp]",
    "mov rdx, rsp",
    "jmp {impl_fn}",
    impl_fn = sym rt_gc_safepoint_slow_impl,
  );
}

// On AArch64 `rt_gc_safepoint` is provided by an assembly stub
// (`arch/aarch64/rt_gc_safepoint.S`) so we can spill the register file before
// running any Rust code.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn rt_gc_safepoint() {
  extern "C" {
    #[link_name = "rt_gc_safepoint"]
    fn rt_gc_safepoint_asm();
  }

  // SAFETY: `rt_gc_safepoint_asm` is a runtime entrypoint that follows the C ABI.
  unsafe { rt_gc_safepoint_asm() }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[no_mangle]
pub extern "C" fn rt_gc_safepoint() {
  extern "C" {
    fn rt_gc_safepoint_unsupported_arch() -> !;
  }
  // SAFETY: this function never returns; it exists solely to produce a clear
  // link-time error on unsupported architectures.
  unsafe { rt_gc_safepoint_unsupported_arch() }
}

/// Visit `(slot, value)` relocation pairs for GC roots reachable from a safepoint.
///
/// `top_callee_fp` must be the frame pointer of the runtime entrypoint frame
/// (`rt_gc_safepoint` / `rt_gc_collect`) at the safepoint callsite.
pub fn visit_reloc_pairs(
  top_callee_fp: u64,
  bounds: Option<StackBounds>,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  visit_reloc_pairs_with_bounds(top_callee_fp, bounds, visit)
}

/// Like [`visit_reloc_pairs`], but allows passing stack bounds for additional safety checks.
///
/// When available, stack bounds help catch bogus frame-pointer chains early by ensuring each frame
/// pointer stays within the stopped thread's stack range.
pub fn visit_reloc_pairs_with_bounds(
  top_callee_fp: u64,
  bounds: Option<StackBounds>,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  let Some(stackmaps) = crate::stackmap::try_stackmaps() else {
    return Ok(());
  };

  let bounds = bounds
    .or_else(|| {
      crate::thread_stack::current_thread_stack_bounds()
        .ok()
        .and_then(|b| StackBounds::new(b.low as u64, b.high as u64).ok())
    })
    .or_else(|| crate::stackwalk::StackBounds::current_thread().ok());

  // Safety: stackmap-driven root enumeration inherently walks raw pointers into thread stacks.
  unsafe {
    crate::walk_gc_roots_from_fp(top_callee_fp, bounds, stackmaps, |slot_addr| {
      let slot = slot_addr as *mut *mut u8;
      // Read the current pointer value in the slot so GC can relocate it.
      let value = slot.read();
      visit(slot, value);
    })
  }
}

/// Visit `(slot, value)` relocation pairs for a safepoint described by a captured [`arch::SafepointContext`].
///
/// This is the variant used when we don't have the runtime callee's frame pointer (e.g. the current
/// thread is the GC coordinator and is not stopped inside `rt_gc_safepoint`), but we *do* have a
/// previously published call-site snapshot (`fp` + return address) for the nearest managed frame.
pub fn visit_reloc_pairs_from_safepoint_context(
  ctx: &arch::SafepointContext,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  visit_reloc_pairs_from_safepoint_context_with_bounds(ctx, None, visit)
}

/// Like [`visit_reloc_pairs_from_safepoint_context`], but allows passing stack bounds for additional safety checks.
pub fn visit_reloc_pairs_from_safepoint_context_with_bounds(
  ctx: &arch::SafepointContext,
  bounds: Option<StackBounds>,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  let Some(stackmaps) = crate::stackmap::try_stackmaps() else {
    return Ok(());
  };

  let bounds = bounds
    .or_else(|| {
      crate::thread_stack::current_thread_stack_bounds()
        .ok()
        .and_then(|b| StackBounds::new(b.low as u64, b.high as u64).ok())
    })
    .or_else(|| crate::stackwalk::StackBounds::current_thread().ok());

  // Safety: stackmap-driven root enumeration inherently walks raw pointers into thread stacks.
  unsafe {
    crate::stackwalk_fp::walk_gc_roots_from_safepoint_context(ctx, bounds, stackmaps, |slot_addr| {
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
  // If we're inside runtime code (e.g. the allocator slow path called
  // `rt_gc_collect`) we may be several runtime frames away from the nearest
  // managed callsite that has a stackmap record. Recover that callsite cursor by
  // walking the FP chain.
  // Call `arch::capture_safepoint_context` directly from this helper (avoid
  // routing it through combinators like `unwrap_or_else`) so the capture
  // implementation can reliably walk out to the *outer* caller frame.
  let ctx = match crate::stackmap::try_stackmaps()
    .and_then(|stackmaps| crate::stackwalk::find_nearest_managed_cursor_from_here(stackmaps))
  {
    Some(cursor) => {
      let sp_callsite = cursor.sp.unwrap_or(0);
      #[cfg(target_arch = "x86_64")]
      let sp_entry = sp_callsite.saturating_sub(crate::arch::WORD_SIZE as u64);
      #[cfg(not(target_arch = "x86_64"))]
      let sp_entry = sp_callsite;

      crate::arch::SafepointContext {
        sp_entry: sp_entry as usize,
        sp: sp_callsite as usize,
        fp: cursor.fp as usize,
        ip: cursor.pc as usize,
      }
    }
    None => arch::capture_safepoint_context(),
  };
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
  if !crate::rt_gc_wait_for_world_stopped_timeout(timeout) {
    threading::safepoint::dump_stop_the_world_timeout(stop_epoch, timeout);
    panic!("world did not reach safepoint in time (timeout={timeout:?}, epoch={stop_epoch})");
  }

  // Root enumeration hook. This is intentionally a "plumbing" step: we count
  // roots to validate that:
  // - stop-the-world coordination works across threads
  // - per-thread safepoint context capture is wired
  // - stackmap lookup / stack walking does not crash when available
  let mut roots = 0usize;
  let _ = threading::safepoint::for_each_root_slot_world_stopped(stop_epoch, |_| roots += 1);
  let _ = roots;

  f();

  // Ensure all threads have observed the resumed epoch before allowing another
  // stop-the-world request. Without this post-resume barrier, a rapid sequence
  // of `rt_gc_collect` calls can start a new stop-the-world epoch before some
  // threads have returned from the previous safepoint slow path, causing them to
  // miss the brief resumed epoch and remain blocked across requests.
  let resume_epoch = crate::rt_gc_resume_world();
  assert!(
    crate::rt_gc_wait_for_world_resumed_timeout(timeout),
    "world did not resume safepoint in time (timeout={timeout:?}, epoch={resume_epoch})"
  );
}

/// Stop the world, enumerate roots, run `f`, and resume.
///
/// This is a coordinator-side API; mutator threads should normally stop via
/// `rt_gc_safepoint`.
pub fn with_world_stopped(f: impl FnOnce()) {
  let stop_epoch = crate::rt_gc_request_stop_the_world();
  with_world_stopped_requested(stop_epoch, f);
}
