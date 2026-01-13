use crate::error::{Error, Result};
use crate::js::{EventLoop, TaskSource};
use crate::web::events::{Event, EventInit, EventTargetId};

pub use crate::web::dom::DocumentReadyState;

/// Category of a subresource that delays the `window` `load` event.
///
/// This is intentionally minimal and can be extended as more subresources are modeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoadBlockerKind {
  /// External scripts (`<script src=...>`) that have been discovered and not yet executed.
  Script,
  /// Render-blocking stylesheets (e.g. `<link rel=stylesheet href=...>`).
  StyleSheet,
  /// Catch-all for future expansion.
  Other,
}

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
  ///
  /// Returns an error when the host cannot currently provide mutable access to the document (for
  /// example, if the lifecycle is invoked before parsing completes).
  fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut crate::dom2::Document) -> R) -> Result<R>;

  /// Notify the lifecycle that HTML parsing has completed.
  ///
  /// This updates `document.readyState` to `interactive` and fires `readystatechange` immediately
  /// (when the state transition occurs), then updates the internal lifecycle state machine so that
  /// `DOMContentLoaded`/`load` can be queued once any pending deferred scripts have executed.
  fn notify_parsing_completed(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()>
  where
    Self: Sized + 'static,
  {
    // HTML sets `document.readyState` to `interactive` once parsing is complete, even if deferred
    // scripts still need to run before `DOMContentLoaded`.
    let ready_state_changed = self.with_dom_mut(|dom| {
      if dom.ready_state() == DocumentReadyState::Loading {
        dom.set_ready_state(DocumentReadyState::Interactive);
        true
      } else {
        false
      }
    })?;

    if ready_state_changed {
      fire_ready_state_change(self, event_loop)?;
    }

    // Queue DOMContentLoaded/load tasks (or mark parsing as complete so they can be queued once any
    // pending deferred scripts have executed).
    self
      .document_lifecycle_mut()
      .parsing_completed(event_loop)?;

    // If parsing completion is signalled from outside an event-loop task turn, perform a microtask
    // checkpoint immediately. This matches the HTML expectation that microtasks queued during the
    // parsing completion steps (e.g. from a `readystatechange` listener) run before the next task.
    //
    // When called from within an event-loop task, `EventLoop::run_next_task` will perform the
    // checkpoint after the task returns, so doing it here would be too early.
    if event_loop.currently_running_task().is_none() {
      event_loop.perform_microtask_checkpoint(self)?;
    }

    Ok(())
  }

  /// Dispatch a DOM event to `target`.
  ///
  /// Hosts should implement this using the canonical event system:
  /// `crate::web::events::dispatch_event`.
  fn dispatch_lifecycle_event(
    &mut self,
    event_loop: &mut EventLoop<Self>,
    target: EventTargetId,
    event: Event,
  ) -> Result<()>
  where
    Self: Sized + 'static;

  /// Give the embedding a chance to discover/register new load blockers right before the `load`
  /// event is dispatched.
  ///
  /// The HTML spec allows scripts and other subresources to be discovered late (for example, scripts
  /// inserted from microtasks after `DOMContentLoaded` has been dispatched but before the `load`
  /// task runs). If the host defers discovery work until "between turns", the queued `load` task may
  /// otherwise run before the new blocker is registered, causing `load` to fire too early.
  ///
  /// Embeddings that need to support late-discovered blockers should override this to perform any
  /// required discovery. The default implementation is a no-op.
  fn before_load_event(&mut self, _event_loop: &mut EventLoop<Self>) -> Result<()>
  where
    Self: Sized + 'static,
  {
    Ok(())
  }

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
/// - a "pending load blockers" gate used to delay `load` until all critical subresources have
///   completed (at minimum: external scripts and render-blocking stylesheets)
///
/// It intentionally does **not** model:
/// - navigation / BFCache
#[derive(Debug, Clone)]
pub struct DocumentLifecycle {
  parsing_completed: bool,
  pending_deferred_scripts: usize,
  pending_load_blockers_script: usize,
  pending_load_blockers_stylesheet: usize,
  pending_load_blockers_other: usize,
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
      pending_load_blockers_script: 0,
      pending_load_blockers_stylesheet: 0,
      pending_load_blockers_other: 0,
      dom_content_loaded_queued: false,
      dom_content_loaded_fired: false,
      load_queued: false,
      load_fired: false,
    }
  }

  fn pending_load_blockers_total(&self) -> usize {
    self
      .pending_load_blockers_script
      .saturating_add(self.pending_load_blockers_stylesheet)
      .saturating_add(self.pending_load_blockers_other)
  }

  /// Returns `true` once `DOMContentLoaded` has been dispatched for this document.
  pub fn dom_content_loaded_fired(&self) -> bool {
    self.dom_content_loaded_fired
  }

  /// Returns `true` once the window `load` event has been dispatched for this document.
  pub fn load_fired(&self) -> bool {
    self.load_fired
  }

  /// Record discovery of a parser-inserted `defer` script that should delay `DOMContentLoaded`.
  pub fn register_deferred_script(&mut self) {
    self.pending_deferred_scripts = self.pending_deferred_scripts.saturating_add(1);
  }

  /// Register a new subresource that should delay `window.load`.
  ///
  /// Hosts should call this when a "load blocker" subresource starts (typically when the fetch for
  /// an external script or render-blocking stylesheet begins).
  pub fn register_pending_load_blocker(&mut self, kind: LoadBlockerKind) {
    match kind {
      LoadBlockerKind::Script => {
        self.pending_load_blockers_script = self.pending_load_blockers_script.saturating_add(1);
      }
      LoadBlockerKind::StyleSheet => {
        self.pending_load_blockers_stylesheet =
          self.pending_load_blockers_stylesheet.saturating_add(1);
      }
      LoadBlockerKind::Other => {
        self.pending_load_blockers_other = self.pending_load_blockers_other.saturating_add(1);
      }
    }
  }

  /// Notify the lifecycle that a previously registered load blocker completed (success or error).
  ///
  /// When this call decrements the last remaining load blocker and `DOMContentLoaded` has already
  /// fired, this queues the `load` task.
  pub fn load_blocker_completed<Host: DocumentLifecycleHost + 'static>(
    &mut self,
    kind: LoadBlockerKind,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    let counter = match kind {
      LoadBlockerKind::Script => &mut self.pending_load_blockers_script,
      LoadBlockerKind::StyleSheet => &mut self.pending_load_blockers_stylesheet,
      LoadBlockerKind::Other => &mut self.pending_load_blockers_other,
    };
    if *counter == 0 {
      return Err(Error::Other(format!(
        "DocumentLifecycle load_blocker_completed underflow for {kind:?}"
      )));
    }
    *counter -= 1;
    self.maybe_queue_load_task(event_loop)
  }

  /// Notify the lifecycle that one deferred script finished executing.
  ///
  /// If parsing is complete and this was the last pending deferred script, this queues the
  /// `DOMContentLoaded` task into the event loop.
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
    self.maybe_queue_dom_content_loaded_task(event_loop)
  }

  /// Notify the lifecycle that HTML parsing has completed.
  ///
  /// This does not immediately fire events; it only queues them once any pending deferred scripts
  /// have executed.
  ///
  /// Note: this method does **not** update `document.readyState`. Embeddings should call
  /// [`DocumentLifecycleHost::notify_parsing_completed`] to perform the `loading` → `interactive`
  /// transition at the correct time (before deferred scripts execute).
  pub fn parsing_completed<Host: DocumentLifecycleHost + 'static>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    self.parsing_completed = true;
    self.maybe_queue_dom_content_loaded_task(event_loop)
  }

  fn maybe_queue_dom_content_loaded_task<Host: DocumentLifecycleHost + 'static>(
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
    event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      // Dispatch DOMContentLoaded within the event loop runtime context, then decide whether `load`
      // can be queued (after listeners have had a chance to register load blockers).
      fire_dom_content_loaded(host, event_loop)?;
      host
        .document_lifecycle_mut()
        .maybe_queue_load_task(event_loop)?;
      Ok(())
    })?;

    Ok(())
  }

  fn maybe_queue_load_task<Host: DocumentLifecycleHost + 'static>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    if self.load_queued || self.load_fired {
      return Ok(());
    }
    if !self.dom_content_loaded_fired {
      return Ok(());
    }
    if self.pending_load_blockers_total() != 0 {
      return Ok(());
    }

    self.load_queued = true;
    event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      maybe_fire_load(host, event_loop)
    })?;
    Ok(())
  }
}

fn fire_dom_content_loaded<Host: DocumentLifecycleHost + 'static>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
) -> Result<()> {
  {
    let lifecycle = host.document_lifecycle_mut();
    if lifecycle.dom_content_loaded_fired {
      return Ok(());
    }
    lifecycle.dom_content_loaded_fired = true;
    lifecycle.dom_content_loaded_queued = false;
  }

  // `document.readyState` transitions to `interactive` once parsing is complete (even before
  // deferred scripts execute). As a defensive fallback for embeddings that forget to call
  // `DocumentLifecycleHost::notify_parsing_completed`, set it here if still `loading` so the
  // DOMContentLoaded event observes the spec state.
  let ready_state_changed = host.with_dom_mut(|dom| {
    if dom.ready_state() == DocumentReadyState::Loading {
      dom.set_ready_state(DocumentReadyState::Interactive);
      true
    } else {
      false
    }
  })?;

  // Fire `readystatechange` whenever `document.readyState` changes.
  if ready_state_changed {
    fire_ready_state_change(host, event_loop)?;
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
  host.dispatch_lifecycle_event(event_loop, EventTargetId::Document, event)?;
  Ok(())
}

fn maybe_fire_load<Host: DocumentLifecycleHost + 'static>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
) -> Result<()> {
  // Let the host discover/register any late load blockers before we decide whether `load` can be
  // dispatched.
  //
  // This is important for resources inserted after `DOMContentLoaded` but before the queued `load`
  // task runs (e.g. scripts inserted via microtasks).
  let load_already_fired = host.document_lifecycle_mut().load_fired;
  if !load_already_fired {
    host.before_load_event(event_loop)?;
  }

  // The `load` task can be queued when the last load blocker reaches 0, but new blockers can still
  // be registered before that queued task runs (for example: scripts inserted from DOMContentLoaded
  // listeners). Re-check the current blocker count at dispatch time and no-op if still blocked.
  {
    let lifecycle = host.document_lifecycle_mut();
    if lifecycle.load_fired {
      return Ok(());
    }
    if !lifecycle.dom_content_loaded_fired || lifecycle.pending_load_blockers_total() != 0 {
      lifecycle.load_queued = false;
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
  })?;

  if ready_state_changed {
    fire_ready_state_change(host, event_loop)?;
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
  host.dispatch_lifecycle_event(event_loop, EventTargetId::Window, event)?;
  Ok(())
}

fn fire_ready_state_change<Host: DocumentLifecycleHost + 'static>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
) -> Result<()> {
  let mut event = Event::new(
    "readystatechange",
    EventInit {
      bubbles: false,
      cancelable: false,
      composed: false,
    },
  );
  event.is_trusted = true;
  host.dispatch_lifecycle_event(event_loop, EventTargetId::Document, event)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::DomJsRealm;
  use crate::js::RunLimits;
  use crate::web::events::{
    dispatch_event, AddEventListenerOptions, DomError, EventListenerInvoker, ListenerId,
  };
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
    fn invoke(
      &mut self,
      listener_id: ListenerId,
      event: &mut Event,
    ) -> std::result::Result<(), DomError> {
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
    fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut crate::dom2::Document) -> R) -> Result<R> {
      Ok(f(&mut self.dom))
    }

    fn dispatch_lifecycle_event(
      &mut self,
      _event_loop: &mut EventLoop<Self>,
      target: EventTargetId,
      mut event: Event,
    ) -> Result<()> {
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

    host.notify_parsing_completed(&mut event_loop)?;

    // `DOMContentLoaded`/`load` must be queued as tasks (not synchronous dispatch). The
    // `readystatechange` event for the `loading` → `interactive` transition is fired immediately
    // when parsing completes.
    assert_eq!(&*log.borrow(), &vec!["rs".to_string()]);
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

    // Barrier task (microtask checkpoint boundary).
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(&*log.borrow(), &vec!["rs".to_string()]);
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

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

    host.notify_parsing_completed(&mut event_loop)?;
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
    host.notify_parsing_completed(&mut event_loop)?;

    // No lifecycle tasks should be queued yet.
    assert!(!event_loop.run_next_task(&mut host)?);
    assert_eq!(&*log.borrow(), &vec!["rs".to_string()]);
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

    // Queue a single "deferred script" task that enqueues a microtask and then signals completion.
    {
      let log = Rc::clone(&log);
      event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
        assert_eq!(host.dom.ready_state().as_str(), "interactive");
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
      &vec![
        "rs".to_string(),
        "script:d1".to_string(),
        "microtask:d1".to_string()
      ]
    );
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

    // Barrier task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs".to_string(),
        "script:d1".to_string(),
        "microtask:d1".to_string()
      ]
    );
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

    // DOMContentLoaded task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs".to_string(),
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "dom".to_string()
      ]
    );
    assert_eq!(host.dom.ready_state().as_str(), "interactive");

    // load task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs".to_string(),
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "dom".to_string(),
        "rs".to_string(),
        "load".to_string()
      ]
    );
    assert_eq!(host.dom.ready_state().as_str(), "complete");

    assert!(!event_loop.run_next_task(&mut host)?);
    Ok(())
  }

  #[test]
  fn dom_content_loaded_does_not_wait_for_async_script_but_load_does() -> Result<()> {
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

    // Simulate an external async script that is still pending.
    host
      .lifecycle
      .register_pending_load_blocker(LoadBlockerKind::Script);
    host.notify_parsing_completed(&mut event_loop)?;

    // Barrier + DOMContentLoaded should run even though the script is still pending.
    assert!(event_loop.run_next_task(&mut host)?);
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(&*log.borrow(), &vec!["dom".to_string()]);

    // `load` must not have been queued yet.
    assert!(!event_loop.run_next_task(&mut host)?);

    // Complete the async script and signal the lifecycle.
    {
      event_loop.queue_task(TaskSource::Script, |host, event_loop| {
        host
          .lifecycle
          .load_blocker_completed(LoadBlockerKind::Script, event_loop)?;
        Ok(())
      })?;
    }

    // Script completion task, then `load`.
    assert!(event_loop.run_next_task(&mut host)?);
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(&*log.borrow(), &vec!["dom".to_string(), "load".to_string()]);
    Ok(())
  }

  #[test]
  fn load_waits_for_blocking_stylesheet() -> Result<()> {
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

    // Simulate a render-blocking stylesheet load.
    host
      .lifecycle
      .register_pending_load_blocker(LoadBlockerKind::StyleSheet);
    host.notify_parsing_completed(&mut event_loop)?;

    // Barrier + DOMContentLoaded should run even though the stylesheet is still pending.
    assert!(event_loop.run_next_task(&mut host)?);
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(&*log.borrow(), &vec!["dom".to_string()]);

    // `load` must not have been queued yet.
    assert!(!event_loop.run_next_task(&mut host)?);

    // Complete stylesheet load and signal the lifecycle.
    {
      event_loop.queue_task(TaskSource::Networking, |host, event_loop| {
        host
          .lifecycle
          .load_blocker_completed(LoadBlockerKind::StyleSheet, event_loop)?;
        Ok(())
      })?;
    }

    // Stylesheet completion task, then `load`.
    assert!(event_loop.run_next_task(&mut host)?);
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(&*log.borrow(), &vec!["dom".to_string(), "load".to_string()]);
    Ok(())
  }

  #[test]
  fn parsing_completed_outside_a_task_turn_drains_microtasks() -> Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let rs_listener = ListenerId::new(1);
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

    // Queue a microtask before signalling parsing completion. When parsing completion is notified
    // from outside an event-loop task turn, the helper should run a microtask checkpoint
    // immediately so these microtasks drain before subsequent tasks.
    {
      let log = Rc::clone(&log);
      event_loop.queue_microtask(move |_host, _event_loop| {
        log.borrow_mut().push("microtask".to_string());
        Ok(())
      })?;
    }

    host.notify_parsing_completed(&mut event_loop)?;

    assert_eq!(
      &*log.borrow(),
      &vec!["rs".to_string(), "microtask".to_string()]
    );
    assert_eq!(event_loop.pending_microtask_count(), 0);
    Ok(())
  }

  fn pk(rt: &mut webidl_js_runtime::VmJsRuntime, name: &str) -> PropertyKey {
    let Value::String(s) = rt.alloc_string_value(name).unwrap() else {
      panic!("alloc_string_value must return a string");
    };
    PropertyKey::String(s)
  }

  fn set_event_handler_property(
    rt: &mut webidl_js_runtime::VmJsRuntime,
    obj: Value,
    name: &str,
    handler: Value,
  ) {
    use webidl_js_runtime::JsPropertyKind;
    let key = pk(rt, name);
    let desc = rt
      .get_own_property(obj, key)
      .expect("get_own_property should succeed")
      .unwrap_or_else(|| panic!("missing expected {name} property"));
    let JsPropertyKind::Accessor { set, .. } = desc.kind else {
      panic!("expected {name} to be an accessor property");
    };
    rt.call_function(set, obj, &[handler])
      .unwrap_or_else(|e| panic!("calling {name} setter failed: {e:?}"));
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
    fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut crate::dom2::Document) -> R) -> Result<R> {
      let dom = self.realm.dom();
      let mut dom_ref = dom.borrow_mut();
      Ok(f(&mut dom_ref))
    }

    fn dispatch_lifecycle_event(
      &mut self,
      _event_loop: &mut EventLoop<Self>,
      target: EventTargetId,
      event: Event,
    ) -> Result<()> {
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

    // Ensure the IDL handler attributes exist and default to null.
    {
      let rt = host.realm.runtime_mut();
      let onload_key = pk(rt, "onload");
      assert_eq!(rt.get(window, onload_key).unwrap(), Value::Null);
      let onrs_key = pk(rt, "onreadystatechange");
      assert_eq!(rt.get(document, onrs_key).unwrap(), Value::Null);
    }

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

    // document.onreadystatechange = ...
    {
      let log = Rc::clone(&log);
      let document_for_cb = document;
      let rt = host.realm.runtime_mut();
      let callback = rt
        .alloc_function_value(move |rt, _this, _args| {
          let ready_state_key = pk(rt, "readyState");
          let ready_state_value = rt.get(document_for_cb, ready_state_key)?;
          let ready_state = value_as_string(rt, ready_state_value);
          log.borrow_mut().push(format!("rsprop:{ready_state}"));
          Ok(Value::Undefined)
        })
        .unwrap();
      set_event_handler_property(rt, document, "onreadystatechange", callback);
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

    // window.onload = ...
    {
      let log = Rc::clone(&log);
      let document_for_cb = document;
      let rt = host.realm.runtime_mut();
      let callback = rt
        .alloc_function_value(move |rt, _this, _args| {
          let ready_state_key = pk(rt, "readyState");
          let ready_state_value = rt.get(document_for_cb, ready_state_key)?;
          let ready_state = value_as_string(rt, ready_state_value);
          log.borrow_mut().push(format!("loadprop:{ready_state}"));
          Ok(Value::Undefined)
        })
        .unwrap();
      set_event_handler_property(rt, window, "onload", callback);
    }

    // Initial state.
    {
      let rt = host.realm.runtime_mut();
      let ready_state_key = pk(rt, "readyState");
      let ready_state_value = rt.get(document, ready_state_key).unwrap();
      let ready_state = value_as_string(rt, ready_state_value);
      assert_eq!(ready_state, "loading");
    }

    host.notify_parsing_completed(&mut event_loop)?;

    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs:interactive".to_string(),
        "rsprop:interactive".to_string()
      ]
    );

    // Barrier task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs:interactive".to_string(),
        "rsprop:interactive".to_string()
      ]
    );

    // DOMContentLoaded task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs:interactive".to_string(),
        "rsprop:interactive".to_string(),
        "dom:interactive".to_string()
      ]
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
      &vec![
        "rs:interactive".to_string(),
        "rsprop:interactive".to_string(),
        "dom:interactive".to_string()
      ]
    );

    // load task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      &*log.borrow(),
      &vec![
        "rs:interactive".to_string(),
        "rsprop:interactive".to_string(),
        "dom:interactive".to_string(),
        "rs:complete".to_string(),
        "rsprop:complete".to_string(),
        "load:complete".to_string(),
        "loadprop:complete".to_string()
      ]
    );

    assert!(!event_loop.run_next_task(&mut host)?);
    Ok(())
  }
}
