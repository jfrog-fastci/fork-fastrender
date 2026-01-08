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
use std::rc::Rc;
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
  pub fn get(self) -> u64 {
    self.0
  }
}

pub trait HostContext {
  fn remove_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    capture: bool,
    listener_id: ListenerId,
  ) -> bool;
}

pub type EventListenerCallback =
  dyn Fn(&mut Event, &mut dyn HostContext) -> std::result::Result<(), DomError>;

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

struct ListenerRecord {
  id: ListenerId,
  type_: String,
  options: AddEventListenerOptions,
  callback: Rc<EventListenerCallback>,
}

#[derive(Clone)]
struct ListenerSnapshot {
  id: ListenerId,
  once: bool,
  passive: bool,
  callback: Rc<EventListenerCallback>,
}

#[derive(Default)]
pub struct EventListenerRegistry {
  next_id: u64,
  listeners: FxHashMap<EventTargetId, Vec<ListenerRecord>>,
}

impl EventListenerRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn add_event_listener<F>(
    &mut self,
    target: EventTargetId,
    type_: impl Into<String>,
    options: AddEventListenerOptions,
    callback: F,
  ) -> ListenerId
  where
    F: Fn(&mut Event, &mut dyn HostContext) -> std::result::Result<(), DomError> + 'static,
  {
    let id = ListenerId(self.next_id);
    self.next_id = self.next_id.wrapping_add(1);
    let record = ListenerRecord {
      id,
      type_: type_.into(),
      options,
      callback: Rc::new(callback),
    };
    self.listeners.entry(target).or_default().push(record);
    id
  }

  pub fn remove_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    capture: bool,
    listener_id: ListenerId,
  ) -> bool {
    let Some(list) = self.listeners.get_mut(&target) else {
      return false;
    };
    let before = list.len();
    list.retain(|record| {
      !(record.id == listener_id && record.options.capture == capture && record.type_ == type_)
    });
    let removed = list.len() != before;
    if list.is_empty() {
      self.listeners.remove(&target);
    }
    removed
  }

  fn snapshot_listeners(&self, target: EventTargetId, type_: &str, capture: bool) -> Vec<ListenerSnapshot> {
    self
      .listeners
      .get(&target)
      .map(|list| {
        list
          .iter()
          .filter(|record| record.type_ == type_ && record.options.capture == capture)
          .map(|record| ListenerSnapshot {
            id: record.id,
            once: record.options.once,
            passive: record.options.passive,
            callback: record.callback.clone(),
          })
          .collect()
      })
      .unwrap_or_default()
  }
}

impl HostContext for EventListenerRegistry {
  fn remove_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    capture: bool,
    listener_id: ListenerId,
  ) -> bool {
    EventListenerRegistry::remove_event_listener(self, target, type_, capture, listener_id)
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
        loop {
          let Some(parent) = dom.node(current).parent else {
            break;
          };
          if matches!(dom.node(parent).kind, dom2::NodeKind::Document { .. }) {
            rev.push(EventTargetId::Document);
            break;
          }
          rev.push(EventTargetId::Node(parent));
          current = parent;
        }
        rev.push(EventTargetId::Window);
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
  registry: &mut EventListenerRegistry,
  capture: bool,
) -> std::result::Result<(), DomError> {
  let snapshot = registry.snapshot_listeners(target, &event.type_, capture);
  for listener in snapshot {
    if event.immediate_propagation_stopped {
      break;
    }
    let prev_passive = event.in_passive_listener;
    event.in_passive_listener = listener.passive;
    let res = (listener.callback)(event, registry as &mut dyn HostContext);
    event.in_passive_listener = prev_passive;

    // `once` listeners are removed after invocation (even if they were already removed by the
    // listener itself).
    if listener.once {
      registry.remove_event_listener(target, &event.type_, capture, listener.id);
    }

    res?;
  }
  Ok(())
}

pub fn dispatch_event(
  target: EventTargetId,
  event: &mut Event,
  dom: &dom2::Document,
  registry: &mut EventListenerRegistry,
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
    invoke_listeners(entry.target, event, registry, /* capture */ true)?;
  }

  // At-target phase: capture listeners then bubble listeners.
  if !event.propagation_stopped {
    let entry = path[target_index];
    event.event_phase = EventPhase::AtTarget;
    event.current_target = Some(entry.target);

    invoke_listeners(entry.target, event, registry, /* capture */ true)?;
    if !event.immediate_propagation_stopped {
      invoke_listeners(entry.target, event, registry, /* capture */ false)?;
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
      invoke_listeners(entry.target, event, registry, /* capture */ false)?;
    }
  }

  event.event_phase = EventPhase::None;
  event.current_target = None;

  Ok(!event.default_prevented)
}

#[cfg(test)]
mod tests;
