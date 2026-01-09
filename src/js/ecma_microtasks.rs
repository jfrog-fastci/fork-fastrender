//! Bridges ECMAScript Promise job scheduling (`vm-js`) onto FastRender's HTML-shaped [`EventLoop`]
//! microtask queue.
//!
//! `vm-js` models Promise jobs as *host-enqueued* microtasks (ECMA-262 `HostEnqueuePromiseJob`,
//! refined by HTML to be queued onto the microtask queue). FastRender already has an HTML-shaped
//! event loop with:
//! - task queues
//! - a microtask queue
//! - explicit microtask checkpoints
//!
//! This module connects the two worlds by implementing `vm-js`'s host hooks in terms of
//! [`EventLoop::queue_microtask`]. As long as the embedding performs microtask checkpoints after
//! script execution (see `script_scheduler`), `vm-js` jobs will be drained at the right times.

use crate::error::Error;

use super::event_loop::EventLoop;

/// Execution context passed to `vm-js` [`vm_js::Job`]s.
///
/// `vm-js` currently models job execution via an opaque [`vm_js::VmJobContext`] trait. The trait is
/// intentionally minimal so it can exist before the full evaluator is implemented.
///
/// FastRender stores the realm that a job was enqueued with so the eventual evaluator integration
/// can re-establish the correct realm/settings object when running the job.
pub struct VmJsJobContext<'a, Host> {
  /// The host value passed to [`EventLoop`] tasks/microtasks.
  pub host: &'a mut Host,
  /// The realm the job was enqueued with (opaque identifier from `vm-js`).
  pub realm: Option<vm_js::RealmId>,
}

impl<Host> vm_js::VmJobContext for VmJsJobContext<'_, Host> {}

/// Adapter implementing `vm-js`'s [`vm_js::VmHostHooks`] by enqueueing jobs into a FastRender
/// [`EventLoop`]'s microtask queue.
///
/// ## Error handling
///
/// `vm-js` host hooks are infallible, but FastRender's [`EventLoop::queue_microtask`] is fallible
/// due to queue limits (the JS runtime is untrusted input).
///
/// When queueing fails, the adapter stores the first error and ignores subsequent jobs. Call
/// [`VmJsHostHooks::take_error`] after script execution to surface the error to the caller.
pub struct VmJsHostHooks<'a, Host: 'static> {
  event_loop: &'a mut EventLoop<Host>,
  enqueue_error: Option<Error>,
}

impl<'a, Host: 'static> VmJsHostHooks<'a, Host> {
  pub fn new(event_loop: &'a mut EventLoop<Host>) -> Self {
    Self {
      event_loop,
      enqueue_error: None,
    }
  }

  pub fn take_error(&mut self) -> Option<Error> {
    self.enqueue_error.take()
  }
}

impl<Host: 'static> vm_js::VmHostHooks for VmJsHostHooks<'_, Host> {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    if self.enqueue_error.is_some() {
      return;
    }

    let result = self.event_loop.queue_microtask(move |host, _event_loop| {
      let mut ctx = VmJsJobContext { host, realm };
      job
        .run(&mut ctx)
        .map_err(|err| Error::Other(format!("vm-js job failed: {err}")))?;
      Ok(())
    });

    if let Err(err) = result {
      self.enqueue_error = Some(err);
    }
  }

  fn host_promise_rejection_tracker(
    &mut self,
    promise: vm_js::PromiseHandle,
    operation: vm_js::PromiseRejectionOperation,
  ) {
    // For now, keep FastRender's host hook behavior identical to the default `vm-js` implementation
    // (no unhandled rejection tracking). Once we have a JS realm + DOM event dispatch, we can map
    // this onto the HTML "about-to-be-notified rejected promises list".
    let _ = (promise, operation);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::event_loop::{RunLimits, RunUntilIdleOutcome, TaskSource};
  use std::sync::{Arc, Mutex};
  use vm_js::VmHostHooks as _;

  #[test]
  fn vm_js_promise_jobs_run_after_a_task_and_before_the_next_task() -> crate::Result<()> {
    #[derive(Clone)]
    struct Host {
      log: Arc<Mutex<Vec<&'static str>>>,
    }

    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let log_for_task = Arc::clone(&log);
    let mut host = Host { log: log.clone() };
    let mut event_loop = EventLoop::<Host>::new();

    // Simulate a script task that enqueues Promise jobs and then queues another task.
    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      host.log.lock().unwrap().push("task1");

      let mut hooks = VmJsHostHooks::new(event_loop);
      let log1 = log_for_task.clone();
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx| {
          log1.lock().unwrap().push("job1");
          Ok(())
        }),
        None,
      );
      let log2 = log_for_task.clone();
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx| {
          log2.lock().unwrap().push("job2");
          Ok(())
        }),
        None,
      );
      let enqueue_err = hooks.take_error();
      drop(hooks);
      if let Some(err) = enqueue_err {
        return Err(err);
      }

      event_loop.queue_task(TaskSource::Timer, |host, _event_loop| {
        host.log.lock().unwrap().push("task2");
        Ok(())
      })?;
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(
      &*log.lock().unwrap(),
      &["task1", "job1", "job2", "task2"]
    );
    Ok(())
  }
}
