//! DOM Events (WHATWG) foundations.
//!
//! This module provides a small, spec-shaped subset of the WHATWG DOM Events system:
//! - `Event` + `EventInit`
//! - `EventTargetId` (stable IDs for host integration)
//! - a listener registry decoupled from `dom2::Node`
//! - a deterministic, spec-shaped `dispatch_event` algorithm
//!
//! Shadow DOM composed paths are intentionally ignored for now, but the event path representation
//! keeps room for extension.

use crate::dom2;
use rustc_hash::FxHashMap;
use std::cell::{Cell, RefCell};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomError {
  #[error("{message}")]
  Message { message: String },
}

impl DomError {
  pub fn new(message: impl Into<String>) -> Self {
    Self::Message {
      message: message.into(),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPhase {
  None,
  Capturing,
  AtTarget,
  Bubbling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventInit {
  pub bubbles: bool,
  pub cancelable: bool,
  pub composed: bool,
}

impl Default for EventInit {
  fn default() -> Self {
    Self {
      bubbles: false,
      cancelable: false,
      composed: false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventTargetId {
  Window,
  Document,
  Node(dom2::NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventListenerOptions {
  pub capture: bool,
}

impl Default for EventListenerOptions {
  fn default() -> Self {
    Self { capture: false }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddEventListenerOptions {
  pub capture: bool,
  pub once: bool,
  pub passive: bool,
}

impl Default for AddEventListenerOptions {
  fn default() -> Self {
    Self {
      capture: false,
      once: false,
      passive: false,
    }
  }
}

impl From<AddEventListenerOptions> for EventListenerOptions {
  fn from(value: AddEventListenerOptions) -> Self {
    Self {
      capture: value.capture,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListenerId(u64);

impl ListenerId {
  pub const fn new(id: u64) -> Self {
    Self(id)
  }

  pub fn get(self) -> u64 {
    self.0
  }
}

pub trait EventListenerInvoker {
  fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> std::result::Result<(), DomError>;
}

#[derive(Debug)]
pub struct Event {
  pub type_: String,
  pub bubbles: bool,
  pub cancelable: bool,
  pub composed: bool,
  pub default_prevented: bool,
  pub propagation_stopped: bool,
  pub immediate_propagation_stopped: bool,
  pub target: Option<EventTargetId>,
  pub current_target: Option<EventTargetId>,
  pub event_phase: EventPhase,
  pub is_trusted: bool,
  in_passive_listener: bool,
}

impl Event {
  pub fn new(type_: impl Into<String>, init: EventInit) -> Self {
    Self {
      type_: type_.into(),
      bubbles: init.bubbles,
      cancelable: init.cancelable,
      composed: init.composed,
      default_prevented: false,
      propagation_stopped: false,
      immediate_propagation_stopped: false,
      target: None,
      current_target: None,
      event_phase: EventPhase::None,
      // Only user agent-dispatched events are trusted; all synthetic events are not.
      is_trusted: false,
      in_passive_listener: false,
    }
  }

  pub fn stop_propagation(&mut self) {
    self.propagation_stopped = true;
  }

  pub fn stop_immediate_propagation(&mut self) {
    self.immediate_propagation_stopped = true;
    self.propagation_stopped = true;
  }

  pub fn prevent_default(&mut self) {
    if !self.cancelable {
      return;
    }
    // In a passive listener, preventDefault() is ignored.
    if self.in_passive_listener {
      return;
    }
    self.default_prevented = true;
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventPathEntry {
  pub target: EventTargetId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredListener {
  record_id: u64,
  id: ListenerId,
  options: AddEventListenerOptions,
}

#[derive(Debug, Default, Clone)]
pub struct EventListenerRegistry {
  next_record_id: Cell<u64>,
  listeners: RefCell<FxHashMap<EventTargetId, FxHashMap<String, Vec<RegisteredListener>>>>,
}

impl EventListenerRegistry {
  fn alloc_record_id(&self) -> u64 {
    let id = self.next_record_id.get();
    self.next_record_id.set(id.wrapping_add(1));
    id
  }

  pub fn new() -> Self {
    Self::default()
  }

  pub fn add_event_listener(
    &self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    options: AddEventListenerOptions,
  ) -> bool {
    let mut listeners = self.listeners.borrow_mut();
    let by_type = listeners.entry(target).or_default();
    let list = by_type.entry(type_.to_string()).or_default();

    if list
      .iter()
      .any(|l| l.id == listener_id && l.options.capture == options.capture)
    {
      return false;
    }

    list.push(RegisteredListener {
      record_id: self.alloc_record_id(),
      id: listener_id,
      options,
    });
    true
  }

  pub fn remove_event_listener(
    &self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    let mut listeners = self.listeners.borrow_mut();
    let Some(by_type) = listeners.get_mut(&target) else {
      return false;
    };
    let Some(list) = by_type.get_mut(type_) else {
      return false;
    };

    let Some(idx) = list
      .iter()
      .position(|l| l.id == listener_id && l.options.capture == capture)
    else {
      return false;
    };

    list.remove(idx);
    if list.is_empty() {
      by_type.remove(type_);
    }
    if by_type.is_empty() {
      listeners.remove(&target);
    }
    true
  }
}

fn build_event_path(target: EventTargetId, dom: &dom2::Document) -> Vec<EventPathEntry> {
  let mut path: Vec<EventTargetId> = Vec::new();
  match target {
    EventTargetId::Window => {
      path.push(EventTargetId::Window);
    }
    EventTargetId::Document => {
      path.push(EventTargetId::Window);
      path.push(EventTargetId::Document);
    }
    EventTargetId::Node(node_id) => {
      // If the caller passes the document node itself, treat it as `Document`.
      if matches!(dom.node(node_id).kind, dom2::NodeKind::Document { .. }) {
        path.push(EventTargetId::Window);
        path.push(EventTargetId::Document);
      } else {
        let mut rev: Vec<EventTargetId> = vec![EventTargetId::Node(node_id)];
        let mut current = node_id;
        let mut reached_document = false;
        loop {
          let Some(parent) = dom.node(current).parent else {
            break;
          };
          if matches!(dom.node(parent).kind, dom2::NodeKind::Document { .. }) {
            rev.push(EventTargetId::Document);
            reached_document = true;
            break;
          }
          rev.push(EventTargetId::Node(parent));
          current = parent;
        }
        if reached_document {
          rev.push(EventTargetId::Window);
        }
        rev.reverse();
        path.extend(rev);
      }
    }
  }

  path.into_iter().map(|target| EventPathEntry { target }).collect()
}

fn invoke_listeners(
  target: EventTargetId,
  event: &mut Event,
  registry: &EventListenerRegistry,
  invoker: &mut dyn EventListenerInvoker,
  capture: bool,
) -> std::result::Result<(), DomError> {
  let listeners = registry.listeners_snapshot(target, &event.type_);
  for listener in listeners {
    if event.immediate_propagation_stopped {
      break;
    }
    if listener.options.capture != capture {
      continue;
    }

    // Honor removals that occur during dispatch.
    //
    // DOM clones the listener list, but retains shared "removed" state. This must be tracked per
    // registration record (not just by `(listener_id, capture)`) so that:
    // - removing a listener and re-adding the "same" callback does not make it run in the same
    //   dispatch (the re-added listener is a new record that is not part of the snapshot).
    if !registry.listener_record_registered(target, &event.type_, listener.record_id) {
      continue;
    }

    if listener.options.once {
      registry.remove_event_listener(
        target,
        &event.type_,
        listener.id,
        listener.options.capture,
      );
    }

    let prev_passive = event.in_passive_listener;
    event.in_passive_listener = listener.options.passive;
    let res = invoker.invoke(listener.id, event);
    event.in_passive_listener = prev_passive;
    res?;
  }
  Ok(())
}

pub fn dispatch_event(
  target: EventTargetId,
  event: &mut Event,
  dom: &dom2::Document,
  registry: &EventListenerRegistry,
  invoker: &mut dyn EventListenerInvoker,
) -> std::result::Result<bool, DomError> {
  // Reset per-dispatch state. DOM permits re-dispatching the same Event instance; state from prior
  // dispatches must not leak.
  event.target = Some(target);
  event.current_target = None;
  event.event_phase = EventPhase::None;
  event.propagation_stopped = false;
  event.immediate_propagation_stopped = false;
  event.in_passive_listener = false;

  let path = build_event_path(target, dom);
  if path.is_empty() {
    return Ok(!event.default_prevented);
  }

  let target_index = path.len() - 1;

  // Capturing phase: Window → ... → parent
  for entry in &path[..target_index] {
    if event.propagation_stopped {
      break;
    }
    event.event_phase = EventPhase::Capturing;
    event.current_target = Some(entry.target);
    invoke_listeners(entry.target, event, registry, invoker, /* capture */ true)?;
  }

  // At-target phase: capture listeners then bubble listeners.
  if !event.propagation_stopped {
    let entry = path[target_index];
    event.event_phase = EventPhase::AtTarget;
    event.current_target = Some(entry.target);

    invoke_listeners(entry.target, event, registry, invoker, /* capture */ true)?;

    if !event.propagation_stopped && !event.immediate_propagation_stopped {
      invoke_listeners(entry.target, event, registry, invoker, /* capture */ false)?;
    }
  }

  // Bubbling phase: parent → ... → Window (only if bubbles)
  if event.bubbles && !event.propagation_stopped {
    for entry in path[..target_index].iter().rev() {
      if event.propagation_stopped {
        break;
      }
      event.event_phase = EventPhase::Bubbling;
      event.current_target = Some(entry.target);
      invoke_listeners(entry.target, event, registry, invoker, /* capture */ false)?;
    }
  }

  event.event_phase = EventPhase::None;
  event.current_target = None;

  Ok(!event.default_prevented)
}

impl EventListenerRegistry {
  fn listener_record_registered(
    &self,
    target: EventTargetId,
    type_: &str,
    record_id: u64,
  ) -> bool {
    self
      .listeners
      .borrow()
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .is_some_and(|listeners| listeners.iter().any(|l| l.record_id == record_id))
  }

  fn listeners_snapshot(&self, target: EventTargetId, type_: &str) -> Vec<RegisteredListener> {
    self
      .listeners
      .borrow()
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .cloned()
      .unwrap_or_default()
  }

  pub(crate) fn contains_listener_id(&self, listener_id: ListenerId) -> bool {
    self.listeners.borrow().values().any(|by_type| {
      by_type
        .values()
        .any(|listeners| listeners.iter().any(|l| l.id == listener_id))
    })
  }
}

#[cfg(test)]
mod tests;
