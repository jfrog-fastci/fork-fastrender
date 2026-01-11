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
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromiseRejectionEvent {
  UnhandledRejection { promise: PromiseRef },
  RejectionHandled { promise: PromiseRef },
}

#[derive(Default)]
struct Tracker {
  about_to_be_notified: Vec<PromiseRef>,
  outstanding_rejected: HashSet<PromiseRef>,
  pending_rejectionhandled: Vec<PromiseRef>,
  events: Vec<PromiseRejectionEvent>,
}

static TRACKER: Lazy<Mutex<Tracker>> = Lazy::new(|| Mutex::new(Tracker::default()));

pub(crate) fn on_reject(promise: PromiseRef) {
  let mut tracker = TRACKER.lock();
  tracker.about_to_be_notified.push(promise);
}

pub(crate) fn on_handle(promise: PromiseRef) {
  let mut tracker = TRACKER.lock();

  if let Some(idx) = tracker
    .about_to_be_notified
    .iter()
    .position(|p| *p == promise)
  {
    tracker.about_to_be_notified.remove(idx);
    return;
  }

  if tracker.outstanding_rejected.remove(&promise) {
    tracker.pending_rejectionhandled.push(promise);
  }
}

pub(crate) fn microtask_checkpoint() {
  let mut tracker = TRACKER.lock();

  if !tracker.about_to_be_notified.is_empty() {
    // Drain the about-to-be-notified list and report any promises that remain unhandled.
    let to_check = std::mem::take(&mut tracker.about_to_be_notified);
    for promise in to_check {
      if crate::async_rt::promise::promise_is_handled(promise) {
        continue;
      }

      tracker
        .events
        .push(PromiseRejectionEvent::UnhandledRejection { promise });
      eprintln!("unhandledrejection: {promise:?}");
      tracker.outstanding_rejected.insert(promise);
    }
  }

  if !tracker.pending_rejectionhandled.is_empty() {
    let to_report = std::mem::take(&mut tracker.pending_rejectionhandled);
    for promise in to_report {
      tracker
        .events
        .push(PromiseRejectionEvent::RejectionHandled { promise });
      eprintln!("rejectionhandled: {promise:?}");
    }
  }
}

pub(crate) fn clear_state_for_tests() {
  let mut tracker = TRACKER.lock();
  tracker.about_to_be_notified.clear();
  tracker.outstanding_rejected.clear();
  tracker.pending_rejectionhandled.clear();
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
