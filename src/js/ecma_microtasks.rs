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
/// `vm-js` jobs need access to a [`vm_js::Vm`] + [`vm_js::Heap`] so they can call/construct
/// functions and manage persistent GC roots while queued.
pub trait VmJsEngineHost {
  /// Borrow the embedded `vm-js` heap immutably.
  fn vm_js_heap(&self) -> &vm_js::Heap;

  /// Borrow the embedded `vm-js` VM and heap mutably using a borrow-splitting API.
  fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap);

  /// Borrow the embedded `vm-js` heap mutably.
  ///
  /// Defaults to the heap returned by [`VmJsEngineHost::vm_js_vm_and_heap_mut`].
  fn vm_js_heap_mut(&mut self) -> &mut vm_js::Heap {
    let (_, heap) = self.vm_js_vm_and_heap_mut();
    heap
  }
}

/// Execution context passed to `vm-js` [`vm_js::Job`]s.
///
/// `vm-js` models job execution via [`vm_js::VmJobContext`], which provides:
/// - `call` / `construct` for invoking JavaScript values, and
/// - `add_root` / `remove_root` for keeping GC handles alive while queued.
///
/// The embedding supplies the underlying [`vm_js::Vm`] + [`vm_js::Heap`] via [`VmJsEngineHost`].
///
/// FastRender also stores the realm that a job was enqueued with so the eventual evaluator
/// integration can re-establish the correct realm/settings object when running the job.
pub struct VmJsJobContext<'a, Host: VmJsEngineHost> {
  /// The host value passed to [`EventLoop`] tasks/microtasks.
  pub host: &'a mut Host,
  /// The realm the job was enqueued with (opaque identifier from `vm-js`).
  pub realm: Option<vm_js::RealmId>,
}

impl<'a, Host: VmJsEngineHost> VmJsJobContext<'a, Host> {
  fn new(host: &'a mut Host, realm: Option<vm_js::RealmId>) -> Self {
    Self { host, realm }
  }
}

impl<Host: VmJsEngineHost> vm_js::VmJobContext for VmJsJobContext<'_, Host> {
  fn call(
    &mut self,
    hooks: &mut dyn vm_js::VmHostHooks,
    callee: vm_js::Value,
    this: vm_js::Value,
    args: &[vm_js::Value],
  ) -> Result<vm_js::Value, vm_js::VmError> {
    let (vm, heap) = self.host.vm_js_vm_and_heap_mut();
    let mut scope = heap.scope();

    // `vm-js` jobs are executed as host work; if a realm is provided, run the call under an
    // execution context bound to that realm.
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.call_with_host(&mut scope, hooks, callee, this, args)
    } else {
      vm.call_with_host(&mut scope, hooks, callee, this, args)
    }
  }

  fn construct(
    &mut self,
    hooks: &mut dyn vm_js::VmHostHooks,
    callee: vm_js::Value,
    args: &[vm_js::Value],
    new_target: vm_js::Value,
  ) -> Result<vm_js::Value, vm_js::VmError> {
    let (vm, heap) = self.host.vm_js_vm_and_heap_mut();
    let mut scope = heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.construct_with_host(&mut scope, hooks, callee, args, new_target)
    } else {
      vm.construct_with_host(&mut scope, hooks, callee, args, new_target)
    }
  }

  fn add_root(&mut self, value: vm_js::Value) -> Result<vm_js::RootId, vm_js::VmError> {
    // Route through `vm_js_heap_mut` so hosts can override which heap stores persistent roots
    // without forcing a `vm_js_vm_and_heap_mut` borrow.
    self.host.vm_js_heap_mut().add_root(value)
  }

  fn remove_root(&mut self, id: vm_js::RootId) {
    self.host.vm_js_heap_mut().remove_root(id);
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
        let mut ctx = VmJsJobContext::new(host, realm);
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
      let mut ctx = VmJsJobContext::new(host, realm);
      let Some(job) = job_cell_for_closure.borrow_mut().take() else {
        // This microtask should run at most once. If the event loop misbehaves, treat the extra run
        // as a no-op rather than panicking (FastRender must not panic in production code).
        return Ok(());
      };
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
      if let Some(job) = job_cell.borrow_mut().take() {
        self.pending_discard.push((job, realm));
      }
      self.enqueue_error = Some(err);
    }
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn vm_js::VmJobContext,
    callback: &vm_js::JobCallback,
    this_argument: vm_js::Value,
    arguments: &[vm_js::Value],
  ) -> Result<vm_js::Value, vm_js::VmError> {
    ctx.call(
      self,
      vm_js::Value::Object(callback.callback()),
      this_argument,
      arguments,
    )
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
  use crate::js::event_loop::{QueueLimits, RunUntilIdleOutcome, TaskSource};
  use crate::js::RunLimits;
  use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};
  use vm_js::VmJobContext as _;
  use vm_js::VmHostHooks as _;

  static JOB_CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);

  fn noop(
    _vm: &mut vm_js::Vm,
    _scope: &mut vm_js::Scope<'_>,
    _host: &mut dyn vm_js::VmHostHooks,
    _callee: vm_js::GcObject,
    _this: vm_js::Value,
    _args: &[vm_js::Value],
  ) -> Result<vm_js::Value, vm_js::VmError> {
    Ok(vm_js::Value::Undefined)
  }

  fn record_callback_call(
    _vm: &mut vm_js::Vm,
    _scope: &mut vm_js::Scope<'_>,
    _host: &mut dyn vm_js::VmHostHooks,
    _callee: vm_js::GcObject,
    _this: vm_js::Value,
    _args: &[vm_js::Value],
  ) -> Result<vm_js::Value, vm_js::VmError> {
    JOB_CALLBACK_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(vm_js::Value::Undefined)
  }

  #[test]
  fn vm_js_promise_jobs_run_after_a_task_and_before_the_next_task() -> crate::Result<()> {
    struct Host {
      log: Arc<Mutex<Vec<&'static str>>>,
      vm: vm_js::Vm,
      heap: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_heap(&self) -> &vm_js::Heap {
        &self.heap
      }

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
    assert_eq!(&*log.lock().unwrap(), &["task1", "job1", "job2", "task2"]);
    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_enqueued_by_jobs_run_in_the_same_microtask_checkpoint() -> crate::Result<()> {
    struct Host {
      log: Arc<Mutex<Vec<&'static str>>>,
      vm: vm_js::Vm,
      heap: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_heap(&self) -> &vm_js::Heap {
        &self.heap
      }

      fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
        (&mut self.vm, &mut self.heap)
      }
    }

    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let limits = vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024);
    let mut host = Host {
      log: log.clone(),
      vm: vm_js::Vm::new(vm_js::VmOptions::default()),
      heap: vm_js::Heap::new(limits),
    };
    let mut event_loop = EventLoop::<Host>::new();

    let mut hooks = VmJsHostHooks::new(&mut event_loop);
    let log_for_job1 = Arc::clone(&log);
    hooks.host_enqueue_promise_job(
      vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, hooks| {
        log_for_job1.lock().unwrap().push("job1");

        // Enqueue a follow-up Promise job while a job is running: this should still be drained by
        // the *same* microtask checkpoint (HTML semantics).
        let log_for_job2 = Arc::clone(&log_for_job1);
        hooks.host_enqueue_promise_job(
          vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
            log_for_job2.lock().unwrap().push("job2");
            Ok(())
          }),
          None,
        );
        Ok(())
      }),
      None,
    );
    assert!(hooks.finish(&mut host).is_none());

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(&*log.lock().unwrap(), &["job1", "job2"]);
    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_discard_persistent_roots_when_enqueue_fails() -> crate::Result<()> {
    let vm_err = |err: vm_js::VmError| Error::Other(format!("vm-js error: {err}"));

    struct Host {
      vm: vm_js::Vm,
      heap: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_heap(&self) -> &vm_js::Heap {
        &self.heap
      }
      fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
        (&mut self.vm, &mut self.heap)
      }
    }

    let limits = vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024);
    let mut host = Host {
      vm: vm_js::Vm::new(vm_js::VmOptions::default()),
      heap: vm_js::Heap::new(limits),
    };
    let mut event_loop = EventLoop::<Host>::new();
    let mut queue_limits = QueueLimits::unbounded();
    queue_limits.max_pending_microtasks = 0;
    event_loop.set_queue_limits(queue_limits);

    let ran1 = Arc::new(AtomicBool::new(false));
    let ran2 = Arc::new(AtomicBool::new(false));

    let mut hooks = VmJsHostHooks::new(&mut event_loop);

    let (root1, job1) = {
      let ran = Arc::clone(&ran1);
      let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
        ran.store(true, Ordering::Relaxed);
        Ok(())
      });
      let mut ctx = VmJsJobContext::new(&mut host, None);
      let root = job
        .add_root(&mut ctx, vm_js::Value::Null)
        .map_err(vm_err)?;
      (root, job)
    };
    hooks.host_enqueue_promise_job(job1, None);
    assert_eq!(host.heap.get_root(root1), Some(vm_js::Value::Null));

    // After a queueing error, additional jobs should be accepted but discarded during `finish`.
    let (root2, job2) = {
      let ran = Arc::clone(&ran2);
      let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
        ran.store(true, Ordering::Relaxed);
        Ok(())
      });
      let mut ctx = VmJsJobContext::new(&mut host, None);
      let root = job
        .add_root(&mut ctx, vm_js::Value::Undefined)
        .map_err(vm_err)?;
      (root, job)
    };
    hooks.host_enqueue_promise_job(job2, None);
    assert_eq!(host.heap.get_root(root2), Some(vm_js::Value::Undefined));

    let err = hooks.finish(&mut host).expect("expected enqueue error");
    let msg = match err {
      Error::Other(msg) => msg,
      other => {
        return Err(Error::Other(format!(
          "expected Error::Other, got {other:?}"
        )));
      }
    };
    assert!(msg.contains("max pending microtasks"), "msg={msg}");

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(
      !ran1.load(Ordering::Relaxed),
      "job should not run when it could not be enqueued"
    );
    assert!(
      !ran2.load(Ordering::Relaxed),
      "job should not run when it could not be enqueued"
    );

    assert_eq!(host.heap.get_root(root1), None);
    assert_eq!(host.heap.get_root(root2), None);
    Ok(())
  }

  #[test]
  fn vm_js_promise_job_failure_is_propagated_to_the_event_loop() {
    struct Host {
      vm: vm_js::Vm,
      heap: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_heap(&self) -> &vm_js::Heap {
        &self.heap
      }

      fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
        (&mut self.vm, &mut self.heap)
      }
    }

    let limits = vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024);
    let mut host = Host {
      vm: vm_js::Vm::new(vm_js::VmOptions::default()),
      heap: vm_js::Heap::new(limits),
    };
    let mut event_loop = EventLoop::<Host>::new();
    let mut hooks = VmJsHostHooks::new(&mut event_loop);
    hooks.host_enqueue_promise_job(
      vm_js::Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| {
        Err(vm_js::VmError::TypeError("boom"))
      }),
      None,
    );
    assert!(hooks.finish(&mut host).is_none());

    let err = event_loop
      .perform_microtask_checkpoint(&mut host)
      .expect_err("expected job failure to surface via microtask checkpoint");
    let msg = match err {
      Error::Other(msg) => msg,
      other => panic!("expected Error::Other, got {other:?}"),
    };
    assert!(
      msg.contains("vm-js job failed: type error: boom"),
      "msg={msg}"
    );
  }

  #[test]
  fn vm_js_job_context_add_root_uses_host_heap_mut_accessor() -> crate::Result<()> {
    let vm_err = |err: vm_js::VmError| Error::Other(format!("vm-js error: {err}"));
    let limits = vm_js::HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024);

    struct Host {
      vm: vm_js::Vm,
      heap_a: vm_js::Heap,
      heap_b: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_heap(&self) -> &vm_js::Heap {
        &self.heap_a
      }

      fn vm_js_heap_mut(&mut self) -> &mut vm_js::Heap {
        &mut self.heap_b
      }

      fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
        (&mut self.vm, &mut self.heap_a)
      }
    }

    let mut host = Host {
      vm: vm_js::Vm::new(vm_js::VmOptions::default()),
      heap_a: vm_js::Heap::new(limits),
      heap_b: vm_js::Heap::new(limits),
    };

    // `VmJsJobContext::add_root` should route through `VmJsEngineHost::vm_js_heap_mut` so hosts can
    // override which heap stores persistent roots without forcing a `vm_js_vm_and_heap_mut` borrow.
    let root_id = {
      let mut ctx = VmJsJobContext {
        host: &mut host,
        realm: None,
      };
      ctx.add_root(vm_js::Value::Null).map_err(vm_err)?
    };

    assert_eq!(host.heap_b.get_root(root_id), Some(vm_js::Value::Null));
    assert_eq!(
      host.heap_a.get_root(root_id),
      None,
      "root should not be stored in heap_a when vm_js_heap_mut is overridden"
    );

    {
      let mut ctx = VmJsJobContext {
        host: &mut host,
        realm: None,
      };
      ctx.remove_root(root_id);
    }
    assert_eq!(host.heap_b.get_root(root_id), None);
    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_root_captured_values_until_run() -> crate::Result<()> {
    let vm_err = |err: vm_js::VmError| Error::Other(format!("vm-js error: {err}"));

    struct Host {
      vm: vm_js::Vm,
      heap: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_heap(&self) -> &vm_js::Heap {
        &self.heap
      }

      fn vm_js_heap_mut(&mut self) -> &mut vm_js::Heap {
        &mut self.heap
      }

      fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
        (&mut self.vm, &mut self.heap)
      }
    }

    let limits = vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024);
    let mut host = Host {
      vm: vm_js::Vm::new(vm_js::VmOptions::default()),
      heap: vm_js::Heap::new(limits),
    };
    let mut event_loop = EventLoop::<Host>::new();

    let call_id = host.vm.register_native_call(noop).map_err(vm_err)?;

    // Queue a PromiseReactionJob that captures heap values, then run GC before the microtask runs.
    // The job should keep the captures alive until it executes and cleans up its roots.
    let callback_obj;
    let argument_obj;
    {
      let mut hooks = VmJsHostHooks::new(&mut event_loop);
      let mut scope = host.heap.scope();

      callback_obj = {
        let name = scope.alloc_string("onFulfilled").map_err(vm_err)?;
        scope
          .alloc_native_function(call_id, None, name, 1)
          .map_err(vm_err)?
      };
      scope
        .push_root(vm_js::Value::Object(callback_obj))
        .map_err(vm_err)?;

      argument_obj = scope.alloc_object().map_err(vm_err)?;
      let argument = vm_js::Value::Object(argument_obj);
      scope.push_root(argument).map_err(vm_err)?;

      let job_callback = hooks.host_make_job_callback(callback_obj);
      let fulfill_reaction = vm_js::PromiseReactionRecord {
        reaction_type: vm_js::PromiseReactionType::Fulfill,
        handler: Some(job_callback),
      };

      let job = vm_js::new_promise_reaction_job(scope.heap_mut(), fulfill_reaction, argument)
        .map_err(vm_err)?;
      hooks.host_enqueue_promise_job(job, None);
      drop(scope);
      assert!(hooks.finish(&mut host).is_none());
    }

    host.heap.collect_garbage();
    assert!(
      host.heap.is_valid_object(callback_obj),
      "Promise job should keep callback object alive until the microtask runs"
    );
    assert!(
      host.heap.is_valid_object(argument_obj),
      "Promise job should keep captured argument alive until the microtask runs"
    );

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    host.heap.collect_garbage();
    assert!(
      !host.heap.is_valid_object(callback_obj),
      "Job::run should remove persistent roots after execution"
    );
    assert!(
      !host.heap.is_valid_object(argument_obj),
      "Job::run should remove persistent roots after execution"
    );

    Ok(())
  }

  #[test]
  fn vm_js_host_call_job_callback_invokes_the_callback() -> crate::Result<()> {
    let call_count_before = JOB_CALLBACK_CALLS.load(Ordering::SeqCst);

    struct Host {
      vm: vm_js::Vm,
      heap: vm_js::Heap,
    }

    impl VmJsEngineHost for Host {
      fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
        (&mut self.vm, &mut self.heap)
      }

      fn vm_js_heap(&self) -> &vm_js::Heap {
        &self.heap
      }
    }

    let limits = vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024);
    let mut host = Host {
      vm: vm_js::Vm::new(vm_js::VmOptions::default()),
      heap: vm_js::Heap::new(limits),
    };
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let callback_func = {
        let (vm, heap) = host.vm_js_vm_and_heap_mut();
        let mut scope = heap.scope();
        let call_id = vm
          .register_native_call(record_callback_call)
          .map_err(|err| Error::Other(format!("register callback failed: {err}")))?;
        let name = scope
          .alloc_string("testCallback")
          .map_err(|err| Error::Other(format!("alloc callback name failed: {err}")))?;
        scope
          .push_root(vm_js::Value::String(name))
          .map_err(|err| Error::Other(format!("push root callback name failed: {err}")))?;
        let func = scope
          .alloc_native_function(call_id, None, name, 0)
          .map_err(|err| Error::Other(format!("alloc callback func failed: {err}")))?;
        scope
          .push_root(vm_js::Value::Object(func))
          .map_err(|err| Error::Other(format!("push root callback func failed: {err}")))?;
        func
      };

      let job_callback = vm_js::JobCallback::new(callback_func);

      let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
        hooks.host_call_job_callback(ctx, &job_callback, vm_js::Value::Undefined, &[])?;
        Ok(())
      });

      // Root the callback function so the captured handle remains valid until the job runs.
      {
        let mut ctx = VmJsJobContext { host, realm: None };
        job
          .add_root(&mut ctx, vm_js::Value::Object(callback_func))
          .map_err(|err| Error::Other(format!("add root failed: {err}")))?;
      }

      let mut hooks = VmJsHostHooks::new(event_loop);
      hooks.host_enqueue_promise_job(job, None);
      if let Some(err) = hooks.finish(host) {
        return Err(err);
      }
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(
      JOB_CALLBACK_CALLS.load(Ordering::SeqCst),
      call_count_before + 1,
      "host_call_job_callback should invoke the callback"
    );
    Ok(())
  }
}
