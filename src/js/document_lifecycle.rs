use crate::error::{Error, Result};
use crate::js::{EventLoop, TaskSource};
use crate::web::events::{Event, EventInit, EventTargetId};

pub use crate::web::dom::DocumentReadyState;

/// Host hooks required by the HTML document lifecycle scheduler.
///
/// This is intentionally minimal: it models the lifecycle algorithm ("queue tasks to dispatch
/// DOMContentLoaded/load") without coupling the scheduler to a specific JS engine. Hosts are
/// responsible for providing an event dispatch implementation (backed by `web::events`) and for
/// exposing the underlying `dom2::Document`.
pub trait DocumentLifecycleHost {
  /// Mutably access the live `dom2` document (for updating `readyState`).
  ///
  /// This is modeled as a callback so hosts can use interior mutability (e.g. `Rc<RefCell<_>>`)
  /// without requiring unsafe lifetime tricks.
  fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut crate::dom2::Document) -> R) -> R;

  /// Dispatch a DOM event to `target`.
  ///
  /// Hosts should implement this using the canonical event system:
  /// `crate::web::events::dispatch_event`.
  fn dispatch_lifecycle_event(&mut self, target: EventTargetId, event: Event) -> Result<()>;

  /// Mutable access to the per-document lifecycle state machine.
  fn document_lifecycle_mut(&mut self) -> &mut DocumentLifecycle;
}

/// Minimal HTML document lifecycle state machine.
///
/// This tracks:
/// - `document.readyState` (stored on `dom2::Document`)
/// - scheduling/firing of `DOMContentLoaded` and `load`
/// - a "deferred scripts pending" gate used to delay `DOMContentLoaded` until all deferred scripts
///   have executed
///
/// It intentionally does **not** model:
/// - subresource loading
/// - `readystatechange`
/// - navigation / BFCache
#[derive(Debug, Clone)]
pub struct DocumentLifecycle {
  parsing_completed: bool,
  pending_deferred_scripts: usize,
  dom_content_loaded_queued: bool,
  dom_content_loaded_fired: bool,
  load_queued: bool,
  load_fired: bool,
}

impl Default for DocumentLifecycle {
  fn default() -> Self {
    Self::new()
  }
}

impl DocumentLifecycle {
  pub fn new() -> Self {
    Self {
      parsing_completed: false,
      pending_deferred_scripts: 0,
      dom_content_loaded_queued: false,
      dom_content_loaded_fired: false,
      load_queued: false,
      load_fired: false,
    }
  }

  /// Record discovery of a parser-inserted `defer` script that should delay `DOMContentLoaded`.
  pub fn register_deferred_script(&mut self) {
    self.pending_deferred_scripts = self.pending_deferred_scripts.saturating_add(1);
  }

  /// Notify the lifecycle that one deferred script finished executing.
  ///
  /// If parsing is complete and this was the last pending deferred script, this queues the
  /// `DOMContentLoaded` and `load` tasks into the event loop.
  pub fn deferred_script_executed<Host: DocumentLifecycleHost + 'static>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    if self.pending_deferred_scripts == 0 {
      return Err(Error::Other(
        "DocumentLifecycle deferred_script_executed underflow".to_string(),
      ));
    }
    self.pending_deferred_scripts -= 1;
    self.maybe_queue_lifecycle_tasks(event_loop)
  }

  /// Notify the lifecycle that HTML parsing has completed.
  ///
  /// This does not immediately fire events; it only queues them once any pending deferred scripts
  /// have executed.
  pub fn parsing_completed<Host: DocumentLifecycleHost + 'static>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    self.parsing_completed = true;
    self.maybe_queue_lifecycle_tasks(event_loop)
  }

  fn maybe_queue_lifecycle_tasks<Host: DocumentLifecycleHost + 'static>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    if !self.parsing_completed {
      return Ok(());
    }
    if self.pending_deferred_scripts != 0 {
      return Ok(());
    }
    if self.dom_content_loaded_queued || self.dom_content_loaded_fired {
      return Ok(());
    }

    self.dom_content_loaded_queued = true;

    // DOMContentLoaded must be queued as a task (not dispatched synchronously) and must run after
    // deferred scripts and a microtask checkpoint. By queueing it only once parsing has completed
    // and no deferred scripts remain (which themselves execute as tasks), and by letting the event
    // loop perform a microtask checkpoint after each task, this ordering falls out naturally.
    //
    // Note: some embeddings mark parsing complete from outside the event loop (e.g. a synchronous
    // streaming parser driver that *queues* script tasks but does not itself run as a task). In
    // that case there may not have been a preceding task turn to provide the required microtask
    // checkpoint boundary. Queue a tiny "barrier" task first so that:
    //   barrier task → microtask checkpoint → DOMContentLoaded task
    // always holds, even when parsing completion is signalled from the host stack.
    event_loop.queue_task(TaskSource::DOMManipulation, |_host, _event_loop| Ok(()))?;
    event_loop.queue_task(TaskSource::DOMManipulation, |host, _event_loop| {
      fire_dom_content_loaded(host)
    })?;

    // `load` must be after DOMContentLoaded and after a microtask checkpoint. Queueing it as a
    // subsequent task ensures the event loop's post-task microtask checkpoint provides the
    // required boundary.
    if !self.load_queued && !self.load_fired {
      self.load_queued = true;
      event_loop.queue_task(TaskSource::DOMManipulation, |host, _event_loop| fire_load(host))?;
    }

    Ok(())
  }
}

fn fire_dom_content_loaded<Host: DocumentLifecycleHost>(host: &mut Host) -> Result<()> {
  {
    let lifecycle = host.document_lifecycle_mut();
    if lifecycle.dom_content_loaded_fired {
      return Ok(());
    }
    lifecycle.dom_content_loaded_fired = true;
    lifecycle.dom_content_loaded_queued = false;
  }

  // `document.readyState` transitions to `interactive` once parsing is complete and deferred
  // scripts have finished executing, immediately before dispatching DOMContentLoaded.
  let ready_state_changed = host.with_dom_mut(|dom| {
    if dom.ready_state() == DocumentReadyState::Loading {
      dom.set_ready_state(DocumentReadyState::Interactive);
      true
    } else {
      false
    }
  });

  // Fire `readystatechange` whenever `document.readyState` changes.
  if ready_state_changed {
    fire_ready_state_change(host)?;
  }

  let mut event = Event::new(
    "DOMContentLoaded",
    EventInit {
      bubbles: true,
      cancelable: false,
      composed: false,
    },
  );
  event.is_trusted = true;
  host.dispatch_lifecycle_event(EventTargetId::Document, event)?;
  Ok(())
}

fn fire_load<Host: DocumentLifecycleHost>(host: &mut Host) -> Result<()> {
  {
    let lifecycle = host.document_lifecycle_mut();
    if lifecycle.load_fired {
      return Ok(());
    }
    lifecycle.load_fired = true;
    lifecycle.load_queued = false;
  }

  // `document.readyState` becomes `complete` immediately before dispatching `load`.
  let ready_state_changed = host.with_dom_mut(|dom| {
    if dom.ready_state() != DocumentReadyState::Complete {
      dom.set_ready_state(DocumentReadyState::Complete);
      true
    } else {
      false
    }
  });

  if ready_state_changed {
    fire_ready_state_change(host)?;
  }

  let mut event = Event::new(
    "load",
    EventInit {
      bubbles: false,
      cancelable: false,
      composed: false,
    },
  );
  event.is_trusted = true;
  host.dispatch_lifecycle_event(EventTargetId::Window, event)?;
  Ok(())
}

fn fire_ready_state_change<Host: DocumentLifecycleHost>(host: &mut Host) -> Result<()> {
  let mut event = Event::new(
    "readystatechange",
    EventInit {
      bubbles: false,
      cancelable: false,
      composed: false,
    },
  );
  event.is_trusted = true;
  host.dispatch_lifecycle_event(EventTargetId::Document, event)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::RunLimits;
  use crate::js::DomJsRealm;
  use crate::web::events::{dispatch_event, AddEventListenerOptions, DomError, EventListenerInvoker, ListenerId};
  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::collections::HashMap;
  use std::rc::Rc;
  use vm_js::{PropertyKey, Value};
  use webidl_js_runtime::JsRuntime as _;

  struct TestInvoker {
    callbacks: HashMap<ListenerId, Box<dyn FnMut(&mut Event) -> std::result::Result<(), DomError>>>,
  }

  impl TestInvoker {
    fn new() -> Self {
      Self {
        callbacks: HashMap::new(),
      }
    }

    fn register(
      &mut self,
      id: ListenerId,
      f: impl FnMut(&mut Event) -> std::result::Result<(), DomError> + 'static,
    ) {
      self.callbacks.insert(id, Box::new(f));
    }
  }

  impl EventListenerInvoker for TestInvoker {
    fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> std::result::Result<(), DomError> {
      let cb = self
        .callbacks
        .get_mut(&listener_id)
        .ok_or_else(|| DomError::new(format!("missing test callback for {listener_id:?}")))?;
      cb(event)
    }
  }

  struct Host {
    dom: crate::dom2::Document,
    lifecycle: DocumentLifecycle,
    invoker: TestInvoker,
  }

  impl Host {
    fn new() -> Self {
      Self {
        dom: crate::dom2::Document::new(QuirksMode::NoQuirks),
        lifecycle: DocumentLifecycle::new(),
        invoker: TestInvoker::new(),
      }
    }
  }

  impl DocumentLifecycleHost for Host {
    fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut crate::dom2::Document) -> R) -> R {
      f(&mut self.dom)
    }

    fn dispatch_lifecycle_event(&mut self, target: EventTargetId, mut event: Event) -> Result<()> {
      let dom: &crate::dom2::Document = &self.dom;
      dispatch_event(target, &mut event, dom, dom.events(), &mut self.invoker)
        .map(|_default_not_prevented| ())
        .map_err(|err| Error::Other(err.to_string()))
    }

    fn document_lifecycle_mut(&mut self) -> &mut DocumentLifecycle {
      &mut self.lifecycle
    }
  }

  #[test]
  fn lifecycle_events_fire_once_in_order_and_ready_state_transitions() -> Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let rs_listener = ListenerId::new(3);
    host.invoker.register(rs_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("rs".to_string());
        Ok(())
      }
    });
    assert!(
      host.dom.events().add_event_listener(
        EventTargetId::Document,
        "readystatechange",
        rs_listener,
        AddEventListenerOptions::default(),
      ),
      "expected readystatechange listener to be inserted",
    );

    let dom_listener = ListenerId::new(1);
    host.invoker.register(dom_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("dom".to_string());
        Ok(())
      }
    });
    assert!(
      host.dom.events().add_event_listener(
        EventTargetId::Document,
        "DOMContentLoaded",
        dom_listener,
        AddEventListenerOptions::default(),
      ),
      "expected DOMContentLoaded listener to be inserted",
    );

    let load_listener = ListenerId::new(2);
    host.invoker.register(load_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("load".to_string());
        Ok(())
      }
    });
    assert!(
      host.dom.events().add_event_listener(
        EventTargetId::Window,
        "load",
        load_listener,
        AddEventListenerOptions::default(),
      ),
      "expected load listener to be inserted",
    );

    assert_eq!(host.dom.ready_state().as_str(), "loading");

    host.lifecycle.parsing_completed(&mut event_loop)?;

    // Must be queued as tasks (not synchronous dispatch).
    assert!(log.borrow().is_empty());
    assert_eq!(host.dom.ready_state().as_str(), "loading");

    // Barrier task (microtask checkpoint boundary).
    assert!(event_loop.run_next_task(&mut host)?);
    assert!(log.borrow().is_empty());
    assert_eq!(host.dom.ready_state().as_str(), "loading");

    // DOMContentLoaded task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(&*log.borrow(), &vec!["rs".to_string(), "dom".to_string()]);
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

    // load task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs".to_string(),
        "dom".to_string(),
        "rs".to_string(),
        "load".to_string()
      ]
    );
    assert_eq!(host.dom.ready_state().as_str(), "complete");

    // No additional firings.
    assert!(!event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs".to_string(),
        "dom".to_string(),
        "rs".to_string(),
        "load".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn adding_dom_content_loaded_listener_after_it_fired_does_not_call_it() -> Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let dom_listener = ListenerId::new(1);
    host.invoker.register(dom_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("dom".to_string());
        Ok(())
      }
    });
    host.dom.events().add_event_listener(
      EventTargetId::Document,
      "DOMContentLoaded",
      dom_listener,
      AddEventListenerOptions::default(),
    );

    host.lifecycle.parsing_completed(&mut event_loop)?;
    assert!(event_loop.run_next_task(&mut host)?); // barrier
    assert!(event_loop.run_next_task(&mut host)?); // DOMContentLoaded

    // Listener added after DOMContentLoaded should not be retroactively invoked.
    let late_listener = ListenerId::new(2);
    host.invoker.register(late_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("late-dom".to_string());
        Ok(())
      }
    });
    host.dom.events().add_event_listener(
      EventTargetId::Document,
      "DOMContentLoaded",
      late_listener,
      AddEventListenerOptions::default(),
    );

    let _ = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(&*log.borrow(), &vec!["dom".to_string()]);
    Ok(())
  }

  #[test]
  fn dom_content_loaded_waits_for_deferred_scripts_and_microtasks() -> Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let rs_listener = ListenerId::new(3);
    host.invoker.register(rs_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("rs".to_string());
        Ok(())
      }
    });
    host.dom.events().add_event_listener(
      EventTargetId::Document,
      "readystatechange",
      rs_listener,
      AddEventListenerOptions::default(),
    );

    let dom_listener = ListenerId::new(1);
    host.invoker.register(dom_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("dom".to_string());
        Ok(())
      }
    });
    host.dom.events().add_event_listener(
      EventTargetId::Document,
      "DOMContentLoaded",
      dom_listener,
      AddEventListenerOptions::default(),
    );

    let load_listener = ListenerId::new(2);
    host.invoker.register(load_listener, {
      let log = Rc::clone(&log);
      move |_event| {
        log.borrow_mut().push("load".to_string());
        Ok(())
      }
    });
    host.dom.events().add_event_listener(
      EventTargetId::Window,
      "load",
      load_listener,
      AddEventListenerOptions::default(),
    );

    // One deferred script pending.
    host.lifecycle.register_deferred_script();
    host.lifecycle.parsing_completed(&mut event_loop)?;

    // No lifecycle tasks should be queued yet.
    assert!(!event_loop.run_next_task(&mut host)?);
    assert!(log.borrow().is_empty());
    assert_eq!(host.dom.ready_state().as_str(), "loading");

    // Queue a single "deferred script" task that enqueues a microtask and then signals completion.
    {
      let log = Rc::clone(&log);
      event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
        log.borrow_mut().push("script:d1".to_string());
        let log_for_micro = Rc::clone(&log);
        event_loop.queue_microtask(move |_host, _event_loop| {
          log_for_micro.borrow_mut().push("microtask:d1".to_string());
          Ok(())
        })?;
        host.lifecycle.deferred_script_executed(event_loop)?;
        Ok(())
      })?;
    }

    // Deferred script task (followed by its microtask checkpoint).
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec!["script:d1".to_string(), "microtask:d1".to_string()]
    );
    assert_eq!(host.dom.ready_state().as_str(), "loading");

    // Barrier task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec!["script:d1".to_string(), "microtask:d1".to_string()]
    );
    assert_eq!(host.dom.ready_state().as_str(), "loading");

    // DOMContentLoaded task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "rs".to_string(),
        "dom".to_string()
      ]
    );
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

    // load task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "rs".to_string(),
        "dom".to_string(),
        "rs".to_string(),
        "load".to_string()
      ]
    );
    assert_eq!(host.dom.ready_state().as_str(), "complete");

    assert!(!event_loop.run_next_task(&mut host)?);
    Ok(())
  }

  fn pk(rt: &mut webidl_js_runtime::VmJsRuntime, name: &str) -> PropertyKey {
    let Value::String(s) = rt.alloc_string_value(name).unwrap() else {
      panic!("alloc_string_value must return a string");
    };
    PropertyKey::String(s)
  }

  fn value_as_string(rt: &webidl_js_runtime::VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string, got {v:?}");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  struct JsHost {
    realm: DomJsRealm,
    lifecycle: DocumentLifecycle,
  }

  impl JsHost {
    fn new() -> Self {
      Self {
        realm: DomJsRealm::new(crate::dom2::Document::new(QuirksMode::NoQuirks)).unwrap(),
        lifecycle: DocumentLifecycle::new(),
      }
    }
  }

  impl DocumentLifecycleHost for JsHost {
    fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut crate::dom2::Document) -> R) -> R {
      let dom = self.realm.dom();
      let mut dom_ref = dom.borrow_mut();
      f(&mut dom_ref)
    }

    fn dispatch_lifecycle_event(&mut self, target: EventTargetId, event: Event) -> Result<()> {
      self
        .realm
        .dispatch_event_to_js(target, event)
        .map(|_default_not_prevented| ())
        .map_err(|e| Error::Other(format!("{e:?}")))
    }

    fn document_lifecycle_mut(&mut self) -> &mut DocumentLifecycle {
      &mut self.lifecycle
    }
  }

  #[test]
  fn lifecycle_events_invoke_js_listeners_and_expose_ready_state() -> Result<()> {
    let mut host = JsHost::new();
    let mut event_loop = EventLoop::<JsHost>::new();
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let document = host.realm.document();
    let window = host.realm.window();

    // document.addEventListener("readystatechange", ...)
    {
      let log = Rc::clone(&log);
      let document_for_cb = document;
      let callback = host
        .realm
        .runtime_mut()
        .alloc_function_value(move |rt, _this, _args| {
          let ready_state_key = pk(rt, "readyState");
          let ready_state_value = rt.get(document_for_cb, ready_state_key)?;
          let ready_state = value_as_string(rt, ready_state_value);
          log.borrow_mut().push(format!("rs:{ready_state}"));
          Ok(Value::Undefined)
        })
        .unwrap();

      let add_key = pk(host.realm.runtime_mut(), "addEventListener");
      let add = host.realm.runtime_mut().get(document, add_key).unwrap();
      let event_type = host
        .realm
        .runtime_mut()
        .alloc_string_value("readystatechange")
        .unwrap();
      host
        .realm
        .runtime_mut()
        .call_function(add, document, &[event_type, callback, Value::Undefined])
        .unwrap();
    }

    // document.addEventListener("DOMContentLoaded", ...)
    {
      let log = Rc::clone(&log);
      let document_for_cb = document;
      let callback = host
        .realm
        .runtime_mut()
        .alloc_function_value(move |rt, _this, _args| {
          let ready_state_key = pk(rt, "readyState");
          let ready_state_value = rt.get(document_for_cb, ready_state_key)?;
          let ready_state = value_as_string(rt, ready_state_value);
          log.borrow_mut().push(format!("dom:{ready_state}"));
          Ok(Value::Undefined)
        })
        .unwrap();

      let add_key = pk(host.realm.runtime_mut(), "addEventListener");
      let add = host.realm.runtime_mut().get(document, add_key).unwrap();
      let event_type = host
        .realm
        .runtime_mut()
        .alloc_string_value("DOMContentLoaded")
        .unwrap();
      host
        .realm
        .runtime_mut()
        .call_function(add, document, &[event_type, callback, Value::Undefined])
        .unwrap();
    }

    // window.addEventListener("load", ...)
    {
      let log = Rc::clone(&log);
      let document_for_cb = document;
      let callback = host
        .realm
        .runtime_mut()
        .alloc_function_value(move |rt, _this, _args| {
          let ready_state_key = pk(rt, "readyState");
          let ready_state_value = rt.get(document_for_cb, ready_state_key)?;
          let ready_state = value_as_string(rt, ready_state_value);
          log.borrow_mut().push(format!("load:{ready_state}"));
          Ok(Value::Undefined)
        })
        .unwrap();

      let add_key = pk(host.realm.runtime_mut(), "addEventListener");
      let add = host.realm.runtime_mut().get(window, add_key).unwrap();
      let event_type = host.realm.runtime_mut().alloc_string_value("load").unwrap();
      host
        .realm
        .runtime_mut()
        .call_function(add, window, &[event_type, callback, Value::Undefined])
        .unwrap();
    }

    // Initial state.
    {
      let rt = host.realm.runtime_mut();
      let ready_state_key = pk(rt, "readyState");
      let ready_state_value = rt.get(document, ready_state_key).unwrap();
      let ready_state = value_as_string(rt, ready_state_value);
      assert_eq!(ready_state, "loading");
    }

    host.lifecycle.parsing_completed(&mut event_loop)?;

    // Barrier task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert!(log.borrow().is_empty());

    // DOMContentLoaded task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec!["rs:interactive".to_string(), "dom:interactive".to_string()]
    );

    // Add a late DOMContentLoaded listener; it must not be invoked retroactively.
    {
      let log = Rc::clone(&log);
      let late_cb = host
        .realm
        .runtime_mut()
        .alloc_function_value(move |_rt, _this, _args| {
          log.borrow_mut().push("late-dom".to_string());
          Ok(Value::Undefined)
        })
        .unwrap();
      let add_key = pk(host.realm.runtime_mut(), "addEventListener");
      let add = host.realm.runtime_mut().get(document, add_key).unwrap();
      let event_type = host
        .realm
        .runtime_mut()
        .alloc_string_value("DOMContentLoaded")
        .unwrap();
      host
        .realm
        .runtime_mut()
        .call_function(add, document, &[event_type, late_cb, Value::Undefined])
        .unwrap();
    }
    assert_eq!(
      &*log.borrow(),
      &vec!["rs:interactive".to_string(), "dom:interactive".to_string()]
    );

    // load task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs:interactive".to_string(),
        "dom:interactive".to_string(),
        "rs:complete".to_string(),
        "load:complete".to_string()
      ]
    );

    assert!(!event_loop.run_next_task(&mut host)?);
    Ok(())
  }
}
