//! Unhandled promise rejection tracking for the runtime-native async runtime.
//!
//! This module implements a small state machine inspired by HTML's
//! `HostPromiseRejectionTracker` integration:
//! <https://html.spec.whatwg.org/multipage/webappapis.html#the-hostpromiserejectiontracker-implementation>
//!
//! `runtime-native` currently exposes a **minimal** promise implementation (sufficient for
//! async/await lowering). Even at this layer it's valuable to track unhandled rejections for
//! debugging / parity with JS semantics:
//! - a rejected promise with no handlers is reported as `unhandledrejection` at a microtask
//!   checkpoint, and
//! - when a previously-unhandled promise becomes handled later, a `rejectionhandled` notification is
//!   reported.
//!
//! Important semantic note: `await` counts as attaching a rejection handler (even if it propagates
//! the error), so awaited promises are treated as "handled".

use crate::abi::PromiseRef;
use crate::async_abi::PromiseHeader;
use crate::gc::HandleId;
use crate::sync::GcAwareMutex;
use once_cell::sync::Lazy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromiseRejectionEvent {
  UnhandledRejection { promise: PromiseRef },
  RejectionHandled { promise: PromiseRef },
}

#[derive(Default)]
struct Tracker {
  about_to_be_notified: Vec<HandleId>,
  outstanding_rejected: Vec<HandleId>,
  pending_rejectionhandled: Vec<HandleId>,
  events: Vec<PromiseRejectionEvent>,
}

static TRACKER: Lazy<GcAwareMutex<Tracker>> = Lazy::new(|| GcAwareMutex::new(Tracker::default()));

#[inline]
fn promise_header_ptr(p: PromiseRef) -> *mut PromiseHeader {
  if p.is_null() {
    return core::ptr::null_mut();
  }
  let header = p.0.cast::<PromiseHeader>();
  if (header as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }
  header
}

fn promise_is_handled_generic(p: PromiseRef) -> bool {
  let header = promise_header_ptr(p);
  if header.is_null() {
    // Null is a "never settles" sentinel and is not eligible for rejection tracking.
    return true;
  }

  // Safety: `header` is non-null and properly aligned; it must point to a `PromiseHeader` prefix
  // because `PromiseRef` is an opaque handle to a promise allocation whose ABI contract requires a
  // `PromiseHeader` at offset 0.
  unsafe { &*header }.is_handled()
}

#[inline]
fn alloc_promise_root(promise: PromiseRef) -> Option<HandleId> {
  if promise.is_null() {
    return None;
  }
  // Root the promise pointer in the persistent handle table so it can be updated across GC moves
  // while it is stored inside this Rust-owned tracker.
  Some(crate::roots::global_persistent_handle_table().alloc(promise.0.cast()))
}

#[inline]
fn promise_from_root(id: HandleId) -> PromiseRef {
  PromiseRef(
    crate::roots::global_persistent_handle_table()
      .get(id)
      .unwrap_or_else(|| std::process::abort())
      .cast(),
  )
}

#[inline]
fn free_promise_root(id: HandleId) {
  let _ = crate::roots::global_persistent_handle_table().free(id);
}

pub(crate) fn on_reject(promise: PromiseRef) {
  let mut tracker = TRACKER.lock();
  if let Some(id) = alloc_promise_root(promise) {
    tracker.about_to_be_notified.push(id);
  }
}

pub(crate) fn on_handle(promise: PromiseRef) {
  let mut tracker = TRACKER.lock();

  if let Some(idx) = tracker
    .about_to_be_notified
    .iter()
    .position(|id| promise_from_root(*id) == promise)
  {
    let id = tracker.about_to_be_notified.remove(idx);
    free_promise_root(id);
    return;
  }

  let handled = tracker
    .outstanding_rejected
    .iter()
    .copied()
    .find(|id| promise_from_root(*id) == promise);
  if let Some(id) = handled {
    tracker.outstanding_rejected.retain(|other| *other != id);
    tracker.pending_rejectionhandled.push(id);
  }
}

pub(crate) fn mark_handled(promise: PromiseRef) {
  let header = promise_header_ptr(promise);
  if header.is_null() {
    return;
  }

  // Safety: see `promise_is_handled_generic`.
  let transitioned = unsafe { &*header }.mark_handled();
  if transitioned {
    on_handle(promise);
  }
}

pub(crate) fn microtask_checkpoint() {
  let mut tracker = TRACKER.lock();

  if !tracker.about_to_be_notified.is_empty() {
    // Drain the about-to-be-notified list and report any promises that remain unhandled.
    let to_check = std::mem::take(&mut tracker.about_to_be_notified);
    for id in to_check {
      let promise = promise_from_root(id);
      if promise_is_handled_generic(promise) {
        free_promise_root(id);
        continue;
      }

      tracker
        .events
        .push(PromiseRejectionEvent::UnhandledRejection { promise });
      eprintln!("unhandledrejection: {promise:?}");
      tracker.outstanding_rejected.push(id);
    }
  }

  if !tracker.pending_rejectionhandled.is_empty() {
    let to_report = std::mem::take(&mut tracker.pending_rejectionhandled);
    for id in to_report {
      let promise = promise_from_root(id);
      tracker
        .events
        .push(PromiseRejectionEvent::RejectionHandled { promise });
      eprintln!("rejectionhandled: {promise:?}");
      free_promise_root(id);
    }
  }
}

pub(crate) fn clear_state_for_tests() {
  let mut tracker = TRACKER.lock();
  for id in tracker.about_to_be_notified.drain(..) {
    free_promise_root(id);
  }
  for id in tracker.outstanding_rejected.drain(..) {
    free_promise_root(id);
  }
  for id in tracker.pending_rejectionhandled.drain(..) {
    free_promise_root(id);
  }
  tracker.events.clear();
}

pub(crate) fn drain_events_for_tests() -> Vec<PromiseRejectionEvent> {
  let mut tracker = TRACKER.lock();
  std::mem::take(&mut tracker.events)
}

pub(crate) fn unhandled_rejection_count_for_tests() -> usize {
  let tracker = TRACKER.lock();
  tracker.about_to_be_notified.len() + tracker.outstanding_rejected.len()
}
