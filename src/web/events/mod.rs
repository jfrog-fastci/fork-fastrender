//! DOM Events (WHATWG) foundations.
//!
//! This module provides a small, spec-shaped subset of the WHATWG DOM Events system:
//! - `Event` + `EventInit`
//! - `EventTargetId` (stable IDs for host integration)
//! - a listener registry decoupled from `dom2::Node`
//! - a deterministic, spec-shaped `dispatch_event` algorithm
//!
//! Shadow DOM is supported for event path construction, retargeting, and `Event.composedPath()`.
//! - `Event.target` is retargeted while dispatch is in progress.
//! - `Event.eventPhase` reports `AT_TARGET` for "shadow-adjusted targets" (e.g. the shadow host)
//!   even when invoked from the capture/bubble loops.
//! - `Event::composed_path()` implements the spec's composed-path algorithm, including filtering
//!   closed shadow trees based on `currentTarget`.

use crate::dom::ShadowRootMode;
use crate::dom2;
use rustc_hash::{FxHashMap, FxHashSet};
use std::any::Any;
use std::cell::{Cell, RefCell};
use thiserror::Error;
use vm_js::Value as JsValue;
use vm_js::{GcObject, Heap, WeakGcObject};

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

#[derive(Debug, Clone, PartialEq)]
pub struct CustomEventInit {
  pub bubbles: bool,
  pub cancelable: bool,
  pub composed: bool,
  pub detail: JsValue,
}

impl Default for CustomEventInit {
  fn default() -> Self {
    Self {
      bubbles: false,
      cancelable: false,
      composed: false,
      // Web spec default is `null`.
      detail: JsValue::Null,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
  Local,
  Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageEventData {
  pub key: Option<String>,
  pub old_value: Option<String>,
  pub new_value: Option<String>,
  pub url: String,
  pub storage_kind: StorageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventTargetId {
  Window,
  Document,
  Node(dom2::NodeId),
  /// An event target that is not part of the DOM tree (e.g. `AbortSignal`, `new EventTarget()`).
  ///
  /// The value is an embedding-defined stable identifier.
  Opaque(u64),
}

impl EventTargetId {
  /// Per spec, the document node is an [`EventTargetId::Document`] (not a `Node`).
  ///
  /// `dom2::NodeId` is an opaque index, but the document node is always index 0.
  pub fn normalize(self) -> Self {
    match self {
      EventTargetId::Node(node_id) if node_id.index() == 0 => EventTargetId::Document,
      other => other,
    }
  }
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

  /// Derives a stable listener identity from a `vm-js` GC object handle.
  ///
  /// `vm-js` GC handles are generation-checked: an `{index, generation}` pair uniquely identifies a
  /// currently-live heap allocation, and the generation changes when a slot is reused. Packing
  /// these parts into a `u64` gives us a deterministic, host-stable identifier for JS callback
  /// identity.
  pub fn from_gc_object(obj: vm_js::GcObject) -> Self {
    // `vm_js::HeapId`'s raw packed `u64` is crate-private; reconstruct the packing here.
    Self::new((obj.index() as u64) | ((obj.generation() as u64) << 32))
  }

  pub fn get(self) -> u64 {
    self.0
  }
}

pub trait EventListenerInvoker {
  fn invoke(
    &mut self,
    listener_id: ListenerId,
    event: &mut Event,
  ) -> std::result::Result<(), DomError>;

  /// Optional hook for invoking the `on{type}` EventHandler property on the dispatch target.
  ///
  /// This is used by embeddings that synthesize JS event objects externally (e.g. FastRender's
  /// `vm-js` integration) and therefore cannot rely on full IDL EventHandler attribute plumbing for
  /// DOM-backed nodes yet.
  ///
  /// The default implementation is a no-op.
  #[inline]
  fn invoke_event_handler_property(
    &mut self,
    _target: EventTargetId,
    _event: &mut Event,
  ) -> std::result::Result<(), DomError> {
    Ok(())
  }

  /// Optional downcasting hook for embeddings that need invoker-specific dynamic context.
  ///
  /// Most invokers are lightweight stack values (and may contain non-`'static` references), so the
  /// default implementation returns `None`. Heap-owned invokers (like FastRender's `vm-js`
  /// integration) can override this to allow the embedding to install per-dispatch dynamic context
  /// without relying on thread-local storage.
  #[inline]
  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    None
  }
}

#[derive(Debug, Clone)]
pub struct Event {
  pub type_: String,
  pub bubbles: bool,
  pub cancelable: bool,
  pub composed: bool,
  /// The time this event instance was created, in milliseconds.
  ///
  /// This roughly corresponds to the DOM `Event.timeStamp` attribute (a `DOMHighResTimeStamp`).
  /// FastRender does not yet have a full "relevant global object" time origin model for events, so
  /// newly constructed events default to `0.0` and embeddings may choose to override the value.
  pub time_stamp: f64,
  pub default_prevented: bool,
  pub propagation_stopped: bool,
  pub immediate_propagation_stopped: bool,
  pub target: Option<EventTargetId>,
  pub current_target: Option<EventTargetId>,
  pub event_phase: EventPhase,
  /// The computed dispatch path for this event.
  ///
  /// This is populated by [`dispatch_event`] while dispatch is in progress so that
  /// `Event.composedPath()` can observe the current propagation path.
  ///
  /// Per WHATWG DOM, the path is cleared (set to the empty list) once dispatch finishes.
  pub path: Vec<EventPathEntry>,
  pub is_trusted: bool,
  /// `CustomEvent.detail` payload.
  ///
  /// For non-`CustomEvent` events, this remains `None`.
  pub detail: Option<JsValue>,
  /// `StorageEvent` payload.
  ///
  /// For non-`storage` events, this remains `None`.
  pub storage: Option<StorageEventData>,
  /// Mouse event payload for `MouseEvent`-backed event types.
  ///
  /// This is populated by host-driven UI input dispatch (e.g. `mousedown`, `mousemove`, ...), and
  /// is used by JS bindings to synthesize `MouseEvent` objects with appropriate fields.
  pub mouse: Option<MouseEvent>,
  /// Drag-and-drop `DataTransfer` payload for `DragEvent`-backed event types.
  ///
  /// When present, JS bindings should synthesize a `DragEvent` (not a `MouseEvent`) and expose this
  /// value via `event.dataTransfer`.
  ///
  /// This is intentionally a raw JS value so embeddings can provide a lightweight host object
  /// without implementing the full HTML `DataTransfer` interface yet.
  ///
  /// MVP: this is currently used for host-driven file drops to expose a placeholder
  /// `dataTransfer.files` list to JS.
  pub drag_data_transfer: Option<JsValue>,
  pub(crate) in_passive_listener: bool,
}

/// Mouse event payload for host-dispatched DOM events.
///
/// This is a lightweight subset of WHATWG UI Events `MouseEvent` fields, sufficient for common
/// real-world event handlers (`clientX/Y`, `button/buttons`, modifier keys, and hover transitions).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
  pub client_x: f64,
  pub client_y: f64,
  /// Button that changed for this event (`MouseEvent.button`).
  pub button: i16,
  /// Currently pressed buttons bitfield (`MouseEvent.buttons`).
  pub buttons: u16,
  /// Click count (`UIEvent.detail`).
  ///
  /// For user input this is typically:
  /// - `1` for the first click in a sequence
  /// - `2` for double-click
  ///
  /// For non-click-related mouse events (move/enter/leave/over/out), this should be `0`.
  pub detail: i32,
  pub ctrl_key: bool,
  pub shift_key: bool,
  pub alt_key: bool,
  pub meta_key: bool,
  /// Related target for hover transition events (`mouseover/out/enter/leave`).
  ///
  /// Best-effort: hosts may set this to `None` when unavailable.
  pub related_target: Option<EventTargetId>,
}

impl Default for MouseEvent {
  fn default() -> Self {
    Self {
      client_x: 0.0,
      client_y: 0.0,
      button: 0,
      buttons: 0,
      detail: 0,
      ctrl_key: false,
      shift_key: false,
      alt_key: false,
      meta_key: false,
      related_target: None,
    }
  }
}

impl Event {
  pub fn new(type_: impl Into<String>, init: EventInit) -> Self {
    Self {
      type_: type_.into(),
      bubbles: init.bubbles,
      cancelable: init.cancelable,
      composed: init.composed,
      time_stamp: 0.0,
      default_prevented: false,
      propagation_stopped: false,
      immediate_propagation_stopped: false,
      target: None,
      current_target: None,
      event_phase: EventPhase::None,
      path: Vec::new(),
      // Only user agent-dispatched events are trusted; all synthetic events are not.
      is_trusted: false,
      detail: None,
      storage: None,
      mouse: None,
      drag_data_transfer: None,
      in_passive_listener: false,
    }
  }

  pub fn new_custom_event(type_: impl Into<String>, init: CustomEventInit) -> Self {
    let mut event = Self::new(
      type_,
      EventInit {
        bubbles: init.bubbles,
        cancelable: init.cancelable,
        composed: init.composed,
      },
    );
    event.detail = Some(init.detail);
    event
  }

  /// Legacy event initializer (`Event.prototype.initEvent`).
  ///
  /// This is intentionally minimal; it exists primarily for compatibility with legacy scripts.
  pub fn init_event(&mut self, type_: impl Into<String>, bubbles: bool, cancelable: bool) {
    self.type_ = type_.into();
    self.bubbles = bubbles;
    self.cancelable = cancelable;
    // `initEvent` does not expose `composed`; default to `false`.
    self.composed = false;

    // Reset state per the DOM "initialize an event" algorithm.
    self.default_prevented = false;
    self.propagation_stopped = false;
    self.immediate_propagation_stopped = false;
    self.target = None;
    self.current_target = None;
    self.event_phase = EventPhase::None;
    self.storage = None;
    self.mouse = None;
    self.drag_data_transfer = None;
  }

  /// Legacy CustomEvent initializer (`CustomEvent.prototype.initCustomEvent`).
  ///
  /// This is intentionally minimal; it exists primarily for compatibility with legacy scripts.
  pub fn init_custom_event(
    &mut self,
    type_: impl Into<String>,
    bubbles: bool,
    cancelable: bool,
    detail: JsValue,
  ) {
    self.init_event(type_, bubbles, cancelable);
    self.detail = Some(detail);
  }

  /// `eventPhase` as the JS-visible numeric constant (0..=3).
  pub fn event_phase_numeric(&self) -> u16 {
    match self.event_phase {
      EventPhase::None => 0,
      EventPhase::Capturing => 1,
      EventPhase::AtTarget => 2,
      EventPhase::Bubbling => 3,
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

  /// Spec-accurate `Event.composedPath()` implementation.
  ///
  /// Notes:
  /// - This is only meaningful while dispatch is in progress (i.e. while `event.path` is
  ///   populated). The dispatch algorithm clears `event.path` before returning.
  /// - Closed shadow roots are filtered based on `event.currentTarget` (callers outside a closed
  ///   shadow tree must not observe its internal nodes).
  pub fn composed_path(&self) -> Vec<EventTargetId> {
    use std::collections::VecDeque;

    let path = &self.path;
    if path.is_empty() {
      return Vec::new();
    }

    let Some(current_target) = self.current_target else {
      // Outside dispatch, `currentTarget` is null and `path` should have been cleared. Prefer a
      // defensive empty result over panicking.
      return Vec::new();
    };

    let mut composed_path: VecDeque<EventTargetId> = VecDeque::new();
    composed_path.push_back(current_target);

    let mut current_target_index: Option<usize> = None;
    let mut current_target_hidden_subtree_level: i32 = 0;

    let mut index: isize = path.len() as isize - 1;
    while index >= 0 {
      let entry = &path[index as usize];
      if entry.root_of_closed_tree {
        current_target_hidden_subtree_level += 1;
      }
      if entry.invocation_target == current_target {
        current_target_index = Some(index as usize);
        break;
      }
      if entry.slot_in_closed_tree {
        current_target_hidden_subtree_level -= 1;
      }
      index -= 1;
    }

    let Some(current_target_index) = current_target_index else {
      // If `currentTarget` is not in `path` (should be impossible), fall back to the trivial list.
      return composed_path.into_iter().collect();
    };

    let mut current_hidden_level: i32 = current_target_hidden_subtree_level;
    let mut max_hidden_level: i32 = current_target_hidden_subtree_level;

    index = current_target_index as isize - 1;
    while index >= 0 {
      let entry = &path[index as usize];
      if entry.root_of_closed_tree {
        current_hidden_level += 1;
      }
      if current_hidden_level <= max_hidden_level {
        composed_path.push_front(entry.invocation_target);
      }
      if entry.slot_in_closed_tree {
        current_hidden_level -= 1;
        if current_hidden_level < max_hidden_level {
          max_hidden_level = current_hidden_level;
        }
      }
      index -= 1;
    }

    current_hidden_level = current_target_hidden_subtree_level;
    max_hidden_level = current_target_hidden_subtree_level;

    index = current_target_index as isize + 1;
    while (index as usize) < path.len() {
      let entry = &path[index as usize];
      if entry.slot_in_closed_tree {
        current_hidden_level += 1;
      }
      if current_hidden_level <= max_hidden_level {
        composed_path.push_back(entry.invocation_target);
      }
      if entry.root_of_closed_tree {
        current_hidden_level -= 1;
        if current_hidden_level < max_hidden_level {
          max_hidden_level = current_hidden_level;
        }
      }
      index += 1;
    }

    composed_path.into_iter().collect()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventPathEntry {
  /// The object whose listeners are invoked.
  pub invocation_target: EventTargetId,
  /// The "shadow-adjusted target" for this path struct (DOM spec).
  ///
  /// A non-null value indicates an at-target invocation (including across shadow boundaries).
  pub shadow_adjusted_target: Option<EventTargetId>,
  pub invocation_target_in_shadow_tree: bool,
  pub root_of_closed_tree: bool,
  pub slot_in_closed_tree: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredListener {
  record_id: u64,
  id: ListenerId,
  options: AddEventListenerOptions,
}

#[derive(Default)]
pub struct EventListenerRegistry {
  next_record_id: Cell<u64>,
  listeners: RefCell<FxHashMap<EventTargetId, FxHashMap<String, Vec<RegisteredListener>>>>,
  /// Parent links for `EventTargetId::Opaque` targets.
  ///
  /// `EventTargetId::Opaque` is used for non-DOM `EventTarget`s created in JS (e.g. `new EventTarget()`),
  /// so the DOM tree can't provide a propagation path. Curated WPT tests rely on a non-standard
  /// extension: `new EventTarget(parent)` creates an EventTarget whose "get the parent" algorithm
  /// returns `parent`, allowing capture/bubble across a synthetic chain.
  opaque_parents: RefCell<FxHashMap<u64, EventTargetId>>,
  /// Weak mapping from opaque target IDs back to their JS object wrappers.
  ///
  /// This allows JS-driven `dispatchEvent()` to:
  /// - expose `event.target/currentTarget` as the correct JS EventTarget object, and
  /// - locate per-target callback roots for non-DOM EventTargets.
  ///
  /// Stored as weak handles so the registry does not keep EventTarget wrappers alive.
  opaque_targets: RefCell<FxHashMap<u64, WeakGcObject>>,
}

impl std::fmt::Debug for EventListenerRegistry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let snapshot = self.listeners.try_borrow().ok().map(|listeners| {
      let targets = listeners.len();
      let event_types = listeners
        .values()
        .map(|by_type| by_type.len())
        .sum::<usize>();
      let listener_count = listeners
        .values()
        .flat_map(|by_type| by_type.values())
        .map(|listeners| listeners.len())
        .sum::<usize>();
      let opaque_parents = self
        .opaque_parents
        .try_borrow()
        .map(|m| m.len())
        .unwrap_or(0);
      let opaque_targets = self
        .opaque_targets
        .try_borrow()
        .map(|m| m.len())
        .unwrap_or(0);
      (
        targets,
        event_types,
        listener_count,
        opaque_parents,
        opaque_targets,
      )
    });

    let mut ds = f.debug_struct("EventListenerRegistry");
    match snapshot {
      Some((targets, event_types, listener_count, opaque_parents, opaque_targets)) => ds
        .field("targets", &targets)
        .field("event_types", &event_types)
        .field("listeners", &listener_count)
        .field("opaque_parents", &opaque_parents)
        .field("opaque_targets", &opaque_targets)
        .finish(),
      None => ds.field("listeners", &"<borrowed>").finish_non_exhaustive(),
    }
  }
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

  /// Returns `true` if there is at least one listener registered for `type_` on `target`.
  pub fn has_event_listeners(&self, target: EventTargetId, type_: &str) -> bool {
    let target = target.normalize();
    self
      .listeners
      .borrow()
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .is_some_and(|listeners| !listeners.is_empty())
  }

  /// Returns `true` if there is at least one listener registered for `type_` on ANY target.
  ///
  /// This is a fast check for scroll performance: if no `wheel` listeners exist anywhere,
  /// we can skip the expensive per-scroll dispatch overhead entirely.
  pub fn has_any_listeners_for_type(&self, type_: &str) -> bool {
    self.listeners.borrow().values().any(|by_type| {
      by_type
        .get(type_)
        .is_some_and(|listeners| !listeners.is_empty())
    })
  }

  /// Returns `true` if an exact listener registration exists.
  ///
  /// This is a lightweight query helper used by integrations that need to detect whether a listener
  /// has been removed (e.g. `{ once: true }` auto-removal) without mutating the registry.
  pub(crate) fn has_listener(
    &self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    let target = target.normalize();
    self
      .listeners
      .borrow()
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .is_some_and(|listeners| {
        listeners
          .iter()
          .any(|l| l.id == listener_id && l.options.capture == capture)
      })
  }

  /// Returns `true` if `dispatch_event` for an event with the given `target`, `type_`, `bubbles`,
  /// and `composed` flags would invoke any listeners.
  ///
  /// This is useful as an optimization for hosts: queueing a task to dispatch an event that cannot
  /// possibly observe a listener is wasted work and can affect deterministic "task turn" tests.
  pub fn has_listeners_for_dispatch(
    &self,
    target: EventTargetId,
    type_: &str,
    dom: &dom2::Document,
    bubbles: bool,
    composed: bool,
  ) -> bool {
    fn has_matching_listener(
      registry: &EventListenerRegistry,
      target: EventTargetId,
      type_: &str,
      capture: bool,
    ) -> bool {
      let target = target.normalize();
      registry
        .listeners
        .borrow()
        .get(&target)
        .and_then(|by_type| by_type.get(type_))
        .is_some_and(|listeners| listeners.iter().any(|l| l.options.capture == capture))
    }

    let event = Event::new(
      type_,
      EventInit {
        bubbles,
        cancelable: false,
        composed,
      },
    );
    let path = build_event_path(target, &event, dom, self);
    if path.is_empty() {
      return false;
    }

    // Capturing pass: iterate path in reverse order.
    for entry in path.iter().rev() {
      if has_matching_listener(self, entry.invocation_target, type_, /* capture */ true) {
        return true;
      }
    }

    // Bubbling pass: iterate path forward.
    for entry in &path {
      if entry.shadow_adjusted_target.is_some() {
        // At-target invocations always run bubble listeners, even when `bubbles=false`.
        if has_matching_listener(self, entry.invocation_target, type_, /* capture */ false) {
          return true;
        }
      } else if bubbles
        && has_matching_listener(self, entry.invocation_target, type_, /* capture */ false)
      {
        return true;
      }
    }

    false
  }

  pub fn add_event_listener(
    &self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    options: AddEventListenerOptions,
  ) -> bool {
    let target = target.normalize();
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

  /// Sets the parent EventTarget for an [`EventTargetId::Opaque`] target.
  ///
  /// This is used by host environments that support manually-linked `EventTarget` graphs (e.g.
  /// `new EventTarget(parent)` in the vm-js WPT harness).
  pub fn set_opaque_parent(&self, child: u64, parent: Option<EventTargetId>) {
    let mut map = self.opaque_parents.borrow_mut();
    match parent {
      Some(parent) => {
        map.insert(child, parent.normalize());
      }
      None => {
        map.remove(&child);
      }
    }
  }

  fn opaque_parent(&self, child: u64) -> Option<EventTargetId> {
    self.opaque_parents.borrow().get(&child).copied()
  }

  pub fn remove_event_listener(
    &self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    let target = target.normalize();
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

  /// Transfers all event listeners registered on node targets from this registry into `dest`,
  /// remapping the node IDs.
  ///
  /// For each `(old, new)` pair in `mapping`:
  /// - removes all listeners registered under `EventTargetId::Node(old)` from `self`
  /// - appends them under `EventTargetId::Node(new)` in `dest` (preserving per-type registration order)
  ///
  /// Only node targets are transferred. `Window`/`Document`/`Opaque` targets are not modified.
  ///
  /// To preserve "removal during dispatch" semantics, transferred listeners are assigned fresh
  /// `record_id`s allocated from the destination registry.
  pub fn transfer_node_listeners(
    &self,
    dest: &EventListenerRegistry,
    mapping: &[(dom2::NodeId, dom2::NodeId)],
  ) {
    if mapping.is_empty() {
      return;
    }

    // Remapping within the same registry should not attempt to borrow the listener RefCell twice.
    // Keep `record_id`s intact in this case: they are already unique within a single registry.
    if std::ptr::eq(self, dest) {
      let mut listeners = self.listeners.borrow_mut();
      let mut extracted: Vec<(EventTargetId, FxHashMap<String, Vec<RegisteredListener>>)> =
        Vec::new();

      for &(old, new) in mapping {
        // `dom2::NodeId` index 0 is the document node; event listeners on that node normalize to
        // `EventTargetId::Document` and are not part of the node-target transfer surface.
        if old.index() == 0 || new.index() == 0 {
          continue;
        }
        let old_target = EventTargetId::Node(old);
        let new_target = EventTargetId::Node(new);
        if let Some(by_type) = listeners.remove(&old_target) {
          extracted.push((new_target, by_type));
        }
      }

      for (new_target, mut by_type) in extracted {
        let dst_by_type = listeners.entry(new_target).or_default();
        for (type_, mut list) in by_type.drain() {
          dst_by_type.entry(type_).or_default().append(&mut list);
        }
      }
      return;
    }

    let extracted: Vec<(EventTargetId, FxHashMap<String, Vec<RegisteredListener>>)> = {
      let mut listeners = self.listeners.borrow_mut();
      let mut extracted: Vec<(EventTargetId, FxHashMap<String, Vec<RegisteredListener>>)> =
        Vec::new();

      for &(old, new) in mapping {
        if old.index() == 0 || new.index() == 0 {
          continue;
        }
        let old_target = EventTargetId::Node(old);
        let new_target = EventTargetId::Node(new);
        if let Some(by_type) = listeners.remove(&old_target) {
          extracted.push((new_target, by_type));
        }
      }

      extracted
    };

    if extracted.is_empty() {
      return;
    }

    let mut dst_listeners = dest.listeners.borrow_mut();
    for (new_target, mut by_type) in extracted {
      let dst_by_type = dst_listeners.entry(new_target).or_default();
      for (type_, list) in by_type.drain() {
        let dst_list = dst_by_type.entry(type_).or_default();
        for listener in list {
          dst_list.push(RegisteredListener {
            record_id: dest.alloc_record_id(),
            ..listener
          });
        }
      }
    }
  }

  pub(crate) fn register_opaque_target(&self, id: u64, target: WeakGcObject) {
    self.opaque_targets.borrow_mut().insert(id, target);
  }

  /// Sweeps dead `EventTargetId::Opaque` targets from the registry.
  ///
  /// `opaque_targets` stores weak handles to JS wrapper objects so the registry does not keep
  /// `EventTarget` wrappers alive. When those wrappers are collected, we must also drop any
  /// associated listener + parent-chain metadata to avoid unbounded growth in long-running runs
  /// (e.g. WPT).
  pub(crate) fn sweep_dead_opaque_targets(&self, heap: &Heap) {
    let mut dead_ids: Vec<u64> = Vec::new();
    // Remove dead weak handles and record their IDs so we can clean up related tables.
    self.opaque_targets.borrow_mut().retain(|&id, weak| {
      if weak.upgrade(heap).is_some() {
        true
      } else {
        dead_ids.push(id);
        false
      }
    });
    if dead_ids.is_empty() {
      return;
    }
    let dead: FxHashSet<u64> = dead_ids.iter().copied().collect();

    // Drop listener tables for dead targets.
    {
      let mut listeners = self.listeners.borrow_mut();
      for id in &dead_ids {
        listeners.remove(&EventTargetId::Opaque(*id));
      }
    }

    // Drop parent links for dead targets, and any links whose parent is now dead.
    self.opaque_parents.borrow_mut().retain(|child, parent| {
      if dead.contains(child) {
        return false;
      }
      match *parent {
        EventTargetId::Opaque(parent_id) => !dead.contains(&parent_id),
        _ => true,
      }
    });
  }

  pub(crate) fn opaque_target_object(&self, heap: &Heap, id: u64) -> Option<GcObject> {
    // Best-effort cleanup: if the object is dead, remove the weak handle so the table doesn't grow
    // unboundedly across repeated allocations.
    let mut map = self.opaque_targets.borrow_mut();
    let weak = *map.get(&id)?;
    match weak.upgrade(heap) {
      Some(obj) => Some(obj),
      None => {
        map.remove(&id);
        // Also drop any associated listener + parent metadata so we don't retain dead opaque ids.
        self.opaque_parents.borrow_mut().remove(&id);
        self.listeners.borrow_mut().remove(&EventTargetId::Opaque(id));
        None
      }
    }
  }
}

impl Clone for EventListenerRegistry {
  fn clone(&self) -> Self {
    // `RefCell`'s derived Clone impl panics if the cell is mutably borrowed. Cloning the registry is
    // only used for snapshotting/testing today, so prefer a best-effort clone over panicking.
    let listeners = self
      .listeners
      .try_borrow()
      .map(|map| map.clone())
      .unwrap_or_default();
    let opaque_parents = self
      .opaque_parents
      .try_borrow()
      .map(|map| map.clone())
      .unwrap_or_default();
    let opaque_targets = self
      .opaque_targets
      .try_borrow()
      .map(|map| map.clone())
      .unwrap_or_default();
    Self {
      next_record_id: Cell::new(self.next_record_id.get()),
      listeners: RefCell::new(listeners),
      opaque_parents: RefCell::new(opaque_parents),
      opaque_targets: RefCell::new(opaque_targets),
    }
  }
}

fn event_target_parent(
  target: EventTargetId,
  event: &Event,
  first_invocation_root: Option<dom2::NodeId>,
  dom: &dom2::Document,
  registry: &EventListenerRegistry,
) -> Option<EventTargetId> {
  match target.normalize() {
    EventTargetId::Window => None,
    // HTML: `load` does not propagate from `document` to `window`.
    EventTargetId::Document => (event.type_ != "load" && dom.has_window_event_parent())
      .then_some(EventTargetId::Window),
    EventTargetId::Node(node_id) => dom
      .get_parent_for_event(node_id, event, first_invocation_root)
      .map(|parent| {
        if parent.index() == 0 {
          EventTargetId::Document
        } else {
          EventTargetId::Node(parent)
        }
      }),
    EventTargetId::Opaque(id) => registry.opaque_parent(id),
  }
}

fn build_event_path(
  target: EventTargetId,
  event: &Event,
  dom: &dom2::Document,
  registry: &EventListenerRegistry,
) -> Vec<EventPathEntry> {
  fn append_to_event_path(
    path: &mut Vec<EventPathEntry>,
    invocation_target: EventTargetId,
    shadow_adjusted_target: Option<EventTargetId>,
    slot_in_closed_tree: bool,
    dom: &dom2::Document,
  ) {
    let invocation_target = invocation_target.normalize();
    let shadow_adjusted_target = shadow_adjusted_target.map(|t| t.normalize());

    let invocation_target_in_shadow_tree = match invocation_target {
      EventTargetId::Node(node_id) => dom.is_shadow_root(dom.event_tree_root(node_id)),
      _ => false,
    };
    let root_of_closed_tree = match invocation_target {
      EventTargetId::Node(node_id) => dom
        .shadow_root_mode(node_id)
        .is_some_and(|mode| mode == ShadowRootMode::Closed),
      _ => false,
    };

    path.push(EventPathEntry {
      invocation_target,
      shadow_adjusted_target,
      invocation_target_in_shadow_tree,
      root_of_closed_tree,
      slot_in_closed_tree,
    });
  }

  let target = target.normalize();
  let target_override = target;

  let first_invocation_root = match target {
    EventTargetId::Node(node_id) => Some(dom.event_tree_root(node_id)),
    _ => None,
  };

  let mut path: Vec<EventPathEntry> = Vec::new();
  let mut seen: FxHashSet<EventTargetId> = FxHashSet::default();

  append_to_event_path(
    &mut path,
    target,
    Some(target_override),
    /* slot_in_closed_tree */ false,
    dom,
  );
  seen.insert(target);

  // Spec-driven path construction, with a hard bound + cycle detection for host-defined parent
  // chains (e.g. `new EventTarget(self)`).
  let mut target_var = target;
  let mut parent = event_target_parent(target_var, event, first_invocation_root, dom, registry);

  for _ in 0..1024 {
    let Some(parent_target) = parent else {
      break;
    };
    let parent_target = parent_target.normalize();
    if !seen.insert(parent_target) {
      break;
    }

    // DOM: If we're traversing from a slottable to its assigned slot, we must mark whether that
    // slot is in a closed shadow tree so `Event.composedPath()` can hide closed shadow internals
    // without hiding the slotted node itself.
    //
    // Spec term: `slot-in-closed-tree` (per-path-entry boolean).
    let slot_in_closed_tree = match (path.last().map(|e| e.invocation_target), parent_target) {
      (Some(EventTargetId::Node(slottable)), EventTargetId::Node(slot)) => {
        if dom.find_slot_for_slottable(slottable, /* open */ false) == Some(slot) {
          dom
            .containing_shadow_root(slot)
            .and_then(|sr| dom.shadow_root_mode(sr))
            .is_some_and(|mode| mode == ShadowRootMode::Closed)
        } else {
          false
        }
      }
      _ => false,
    };

    // Shadow DOM retargeting only applies to node targets. For other parent chains (including
    // `EventTargetId::Opaque`), behave like a normal ancestor chain so there is only a single
    // at-target invocation.
    let same_tree_root = match parent_target {
      EventTargetId::Window => true,
      EventTargetId::Node(parent_node_id) => match target_var {
        EventTargetId::Node(target_node_id) => {
          let target_root = dom.event_tree_root(target_node_id);
          dom.is_shadow_including_inclusive_ancestor(target_root, parent_node_id)
        }
        _ => true,
      },
      _ => true,
    };

    if same_tree_root {
      append_to_event_path(&mut path, parent_target, None, slot_in_closed_tree, dom);
    } else {
      target_var = parent_target;
      append_to_event_path(
        &mut path,
        parent_target,
        Some(target_var),
        slot_in_closed_tree,
        dom,
      );
    }

    parent = event_target_parent(parent_target, event, first_invocation_root, dom, registry);
  }

  path
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
      registry.remove_event_listener(target, &event.type_, listener.id, listener.options.capture);
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
  let target = target.normalize();
  // Reset per-dispatch state. DOM permits re-dispatching the same Event instance; state from prior
  // dispatches must not leak.
  event.target = None;
  event.current_target = None;
  event.event_phase = EventPhase::None;
  event.propagation_stopped = false;
  event.immediate_propagation_stopped = false;
  event.in_passive_listener = false;

  event.path = build_event_path(target, event, dom, registry);
  if event.path.is_empty() {
    return Ok(!event.default_prevented);
  }

  // Precompute the "effective" event.target for each path entry per the DOM `invoke` algorithm.
  //
  // This implements per-invocation retargeting across shadow boundaries: for a given invocation
  // struct, the event's `target` is the last path entry at or before that struct whose
  // `shadow_adjusted_target` is non-null.
  let mut last_shadow_adjusted_target: Option<EventTargetId> = None;
  let mut shadow_adjusted_prefix: Vec<Option<EventTargetId>> = Vec::with_capacity(event.path.len());
  for entry in &event.path {
    if let Some(t) = entry.shadow_adjusted_target {
      last_shadow_adjusted_target = Some(t);
    }
    shadow_adjusted_prefix.push(last_shadow_adjusted_target);
  }

  let dispatch_res: std::result::Result<(), DomError> = (|| {
    // Capturing pass: iterate path in reverse order.
    for idx in (0..event.path.len()).rev() {
      let entry = event.path[idx];
      event.target = shadow_adjusted_prefix[idx];
      event.event_phase = if entry.shadow_adjusted_target.is_some() {
        EventPhase::AtTarget
      } else {
        EventPhase::Capturing
      };
      // DOM: the "stop propagation" flag is checked at the start of each `invoke` step.
      //
      // When it is set we still continue walking the path (so `event.target` can be retargeted for
      // subsequent structs), but skip invoking listeners.
      if event.propagation_stopped {
        continue;
      }
      event.current_target = Some(entry.invocation_target);
      invoke_listeners(
        entry.invocation_target,
        event,
        registry,
        invoker,
        /* capture */ true,
      )?;
    }

    // Bubbling pass: iterate path in tree order.
    for idx in 0..event.path.len() {
      let entry = event.path[idx];
      if entry.shadow_adjusted_target.is_some() {
        event.event_phase = EventPhase::AtTarget;
      } else {
        if !event.bubbles {
          continue;
        }
        event.event_phase = EventPhase::Bubbling;
      }

      event.target = shadow_adjusted_prefix[idx];

      // DOM: the "stop propagation" flag is checked at the start of each `invoke` step.
      //
      // When it is set we still continue walking the path (so `event.target` can be retargeted for
      // subsequent structs), but skip invoking listeners.
      if event.propagation_stopped {
        continue;
      }

      event.current_target = Some(entry.invocation_target);
      invoke_listeners(
        entry.invocation_target,
        event,
        registry,
        invoker,
        /* capture */ false,
      )?;

      // EventHandler IDL attribute / handler property (e.g. `node.onclick = fn`).
      //
      // This runs after the regular bubble listeners on the same target so:
      // - it participates in propagation control (`stopPropagation` / `stopImmediatePropagation`), and
      // - it can observe state changes made by `addEventListener` callbacks.
      //
      // Handler properties participate in the bubbling pass:
      // - the at-target invocation always runs (even when `event.bubbles` is `false`), and
      // - ancestor invocations run only when `event.bubbles` is `true` (enforced above).
      if !event.immediate_propagation_stopped {
        invoker.invoke_event_handler_property(entry.invocation_target, event)?;
      }
    }

    Ok(())
  })();

  let default_not_prevented = !event.default_prevented;

  event.event_phase = EventPhase::None;
  event.current_target = None;
  // The DOM dispatch algorithm clears the computed path at the end of dispatch.
  event.path.clear();
  // Align with the DOM Standard: stop-propagation flags are per-dispatch and are cleared once the
  // dispatch finishes.
  event.propagation_stopped = false;
  event.immediate_propagation_stopped = false;
  event.in_passive_listener = false;

  dispatch_res?;
  Ok(default_not_prevented)
}

impl EventListenerRegistry {
  fn listener_record_registered(&self, target: EventTargetId, type_: &str, record_id: u64) -> bool {
    let target = target.normalize();
    self
      .listeners
      .borrow()
      .get(&target)
      .and_then(|by_type| by_type.get(type_))
      .is_some_and(|listeners| listeners.iter().any(|l| l.record_id == record_id))
  }

  fn listeners_snapshot(&self, target: EventTargetId, type_: &str) -> Vec<RegisteredListener> {
    let target = target.normalize();
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

  pub(crate) fn contains_listener_id_for_target(
    &self,
    target: EventTargetId,
    listener_id: ListenerId,
  ) -> bool {
    let target = target.normalize();
    self.listeners.borrow().get(&target).is_some_and(|by_type| {
      by_type
        .values()
        .any(|listeners| listeners.iter().any(|l| l.id == listener_id))
    })
  }
}

#[cfg(test)]
mod tests;
