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
use std::cell::RefCell;
use std::rc::Rc;

/// Trait for event-loop hosts that embed a `vm-js` VM + heap.
///
/// `vm-js` Promise jobs require the ability to:
/// - call/construct JS values (via [`vm_js::Vm`]),
/// - and keep GC handles alive while queued (persistent roots on [`vm_js::Heap`]).
pub trait VmJsEngineHost {
  fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap);
}

/// Execution context passed to `vm-js` job closures.
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

impl<Host: VmJsEngineHost> vm_js::VmJobContext for VmJsJobContext<'_, Host> {
  fn call(
    &mut self,
    callee: vm_js::Value,
    this: vm_js::Value,
    args: &[vm_js::Value],
  ) -> Result<vm_js::Value, vm_js::VmError> {
    let (vm, heap) = self.host.vm_js_vm_and_heap_mut();
    let mut scope = heap.scope();
    vm.call(&mut scope, callee, this, args)
  }

  fn construct(
    &mut self,
    callee: vm_js::Value,
    args: &[vm_js::Value],
    new_target: vm_js::Value,
  ) -> Result<vm_js::Value, vm_js::VmError> {
    let (vm, heap) = self.host.vm_js_vm_and_heap_mut();
    let mut scope = heap.scope();
    vm.construct(&mut scope, callee, args, new_target)
  }

  fn add_root(&mut self, value: vm_js::Value) -> vm_js::RootId {
    let (_vm, heap) = self.host.vm_js_vm_and_heap_mut();
    heap.add_root(value)
  }

  fn remove_root(&mut self, id: vm_js::RootId) {
    let (_vm, heap) = self.host.vm_js_vm_and_heap_mut();
    heap.remove_root(id)
  }
}

/// Adapter implementing `vm-js`'s [`vm_js::VmHostHooks`] by enqueueing jobs into a FastRender
/// [`EventLoop`]'s microtask queue.
///
/// ## Error handling
///
/// `vm-js` host hooks are infallible, but FastRender's [`EventLoop::queue_microtask`] is fallible
/// due to queue limits (the JS runtime is untrusted input).
///
/// When queueing fails, the adapter stores the first error and ignores subsequent jobs. Call
/// [`VmJsHostHooks::finish`] after script execution to surface the error to the caller and tear
/// down any jobs that failed to enqueue.
pub struct VmJsHostHooks<'a, Host: VmJsEngineHost + 'static> {
  event_loop: &'a mut EventLoop<Host>,
  pending_discard: Vec<(vm_js::Job, Option<vm_js::RealmId>)>,
  enqueue_error: Option<Error>,
}

impl<'a, Host: VmJsEngineHost + 'static> VmJsHostHooks<'a, Host> {
  pub fn new(event_loop: &'a mut EventLoop<Host>) -> Self {
    Self {
      event_loop,
      pending_discard: Vec::new(),
      enqueue_error: None,
    }
  }

  /// Finish using this host hook adapter.
  ///
  /// This discards any jobs that could not be enqueued (cleaning up any persistent roots they
  /// captured) and returns the first queueing error (if any).
  pub fn finish(mut self, host: &mut Host) -> Option<Error> {
    if !self.pending_discard.is_empty() {
      for (job, realm) in self.pending_discard.drain(..) {
        let mut ctx = VmJsJobContext { host, realm };
        job.discard(&mut ctx);
      }
    }
    self.enqueue_error.take()
  }
}

impl<Host: VmJsEngineHost + 'static> vm_js::VmHostHooks for VmJsHostHooks<'_, Host> {
  fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
    if self.enqueue_error.is_some() {
      self.pending_discard.push((job, realm));
      return;
    }

    let job_cell: Rc<RefCell<Option<vm_js::Job>>> = Rc::new(RefCell::new(Some(job)));
    let job_cell_for_closure = Rc::clone(&job_cell);

    let result = self.event_loop.queue_microtask(move |host, event_loop| {
      // Promise jobs can enqueue additional Promise jobs (e.g. thenable chains). Provide a fresh
      // host hook adapter for each run so nested jobs are queued onto the same microtask queue.
      let mut hooks = VmJsHostHooks::new(event_loop);
      let mut ctx = VmJsJobContext { host, realm };
      let job = job_cell_for_closure
        .borrow_mut()
        .take()
        .expect("vm-js promise job should run at most once");
      let job_result =
        job.run(&mut ctx, &mut hooks)
          .map_err(|err| Error::Other(format!("vm-js job failed: {err}")));
      drop(ctx);

      let enqueue_err = hooks.finish(host);
      if let Some(err) = enqueue_err {
        return Err(err);
      }
      job_result?;
      Ok(())
    });

    if let Err(err) = result {
      if let Some(mut job) = job_cell.borrow_mut().take() {
        self.pending_discard.push((job, realm));
      }
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
  use crate::js::event_loop::{RunUntilIdleOutcome, TaskSource};
  use crate::js::RunLimits;
  use std::sync::{Arc, Mutex};
  use vm_js::VmHostHooks as _;

  #[test]
  fn vm_js_promise_jobs_run_after_a_task_and_before_the_next_task() -> crate::Result<()> {
    struct Host {
      log: Arc<Mutex<Vec<&'static str>>>,
      vm: vm_js::Vm,
      heap: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
        (&mut self.vm, &mut self.heap)
      }
    }

    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let log_for_task = Arc::clone(&log);
    let limits = vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024);
    let mut host = Host {
      log: log.clone(),
      vm: vm_js::Vm::new(vm_js::VmOptions::default()),
      heap: vm_js::Heap::new(limits),
    };
    let mut event_loop = EventLoop::<Host>::new();

    // Simulate a script task that enqueues Promise jobs and then queues another task.
    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      host.log.lock().unwrap().push("task1");

      let mut hooks = VmJsHostHooks::new(event_loop);
      let log1 = log_for_task.clone();
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
          log1.lock().unwrap().push("job1");
          Ok(())
        }),
        None,
      );
      let log2 = log_for_task.clone();
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
          log2.lock().unwrap().push("job2");
          Ok(())
        }),
        None,
      );
      let enqueue_err = hooks.finish(host);
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
