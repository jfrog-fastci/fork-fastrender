use crate::css::selectors::FastRenderSelectorImpl;
use crate::dom::HTML_NAMESPACE;
use crate::dom::{DomNode, DomNodeType, ShadowRootMode};
use crate::web::dom::selectors::{node_matches_selector_list, parse_selector_list_for_dom};
use crate::web::dom::DocumentReadyState;
use crate::web::dom::DomException;
use crate::web::events as web_events;
use rustc_hash::{FxHashMap, FxHashSet};
use selectors::context::QuirksMode;
use selectors::matching::SelectorCaches;
use selectors::parser::SelectorList;
use selectors::OpaqueElement;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

mod attrs;
mod class_list;
mod cross_document;
mod error;
pub use error::{DomError, Result as DomResult};
mod qualified_name;
pub use qualified_name::{ParsedQualifiedName, XMLNS_NAMESPACE, XML_NAMESPACE};
pub(crate) use qualified_name::{
  validate_and_extract_attribute, validate_and_extract_element, validate_attribute_local_name,
  validate_attribute_qualified_name, validate_element_qualified_name,
};
pub use cross_document::NodeIdMapping;

mod dom_parsing;
mod file_input_safety;
mod form_controls;
mod html;
mod html5ever_tree_sink;
mod html_fragment_parse;
mod html_parse;
pub mod import;
mod js_shims;
mod live_collection_query;
mod live_mutation;
mod mutation;
mod mutation_observer;
mod intersection_observer;
mod resize_observer;
mod range;
mod scripting_parser;
mod xml_parse;
mod serialization;
mod xml_serialization;
mod shadow_dom;
mod slotting;
mod style_attr;
mod traversal;
pub use html5ever_tree_sink::Dom2TreeSink;
pub use html_parse::{parse_html, parse_html_with_options};
pub use cross_document::{clone_node_into_document, clone_node_into_document_deep, AdoptedSubtree};
pub use xml_parse::parse_xml;
pub use range::{BoundaryPoint, RangeId};
pub(crate) use range::cmp_dom2_nodes;

pub use mutation_observer::{
  MutationObserverAgent, MutationObserverId, MutationObserverInit, MutationObserverLimits, MutationRecord,
  MutationRecordType,
};
pub use intersection_observer::{
  IntersectionObserverEntry, IntersectionObserverId, IntersectionObserverInit, IntersectionObserverLimits,
};
pub use resize_observer::{
  ResizeObserverBoxOptions, ResizeObserverEntry, ResizeObserverId, ResizeObserverLimits, ResizeObserverSize,
};
pub use scripting_parser::parse_html_with_scripting_dom2;
pub use live_mutation::{LiveRangeId, NodeIteratorId};
pub(crate) use live_mutation::MovedLiveRange;
#[cfg(test)]
pub(crate) use live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
pub use slotting::SlotAssignmentMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

impl NodeId {
  pub(crate) fn from_index(index: usize) -> Self {
    Self(index)
  }

  pub fn index(self) -> usize {
    self.0
  }
}

#[derive(Debug, Clone)]
struct NodeIteratorState {
  root: NodeId,
  reference: NodeId,
  pointer_before_reference: bool,
}

/// Internal sentinel namespace string representing "no namespace" (`null` in DOM terms).
///
/// `dom2` historically stores the HTML namespace as an empty string to match the renderer's DOM
/// representation. Since the DOM Standard distinguishes between the HTML namespace and a `null`
/// namespace, we need a third value that cannot collide with real namespace URIs.
///
/// This sentinel is never exposed directly to JS; bindings map it back to `null` for
/// `namespaceURI`, and ensure it is not treated as the HTML namespace for case-insensitive matching.
pub const NULL_NAMESPACE: &str = "\u{0000}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
  /// Internal namespace string for the attribute.
  ///
  /// Uses the same sentinel scheme as element namespaces:
  /// - [`NULL_NAMESPACE`] represents a DOM `null` namespace.
  /// - `""` represents the HTML namespace (XHTML) for compatibility with FastRender's renderer DOM.
  /// - Any other string is stored verbatim.
  pub namespace: String,
  pub prefix: Option<String>,
  pub local_name: String,
  pub value: String,
}

impl Attribute {
  pub fn new(namespace: &str, prefix: Option<&str>, local_name: &str, value: &str) -> Self {
    Self {
      namespace: namespace.to_string(),
      prefix: prefix.map(|p| p.to_string()),
      local_name: local_name.to_string(),
      value: value.to_string(),
    }
  }

  pub fn new_no_namespace(local_name: &str, value: &str) -> Self {
    Self::new(NULL_NAMESPACE, None, local_name, value)
  }

  pub fn qualified_name(&self) -> std::borrow::Cow<'_, str> {
    match self.prefix.as_deref() {
      Some(prefix) => std::borrow::Cow::Owned(format!("{prefix}:{}", self.local_name)),
      None => std::borrow::Cow::Borrowed(self.local_name.as_str()),
    }
  }

  pub fn qualified_name_matches(&self, query: &str, is_html: bool) -> bool {
    match self.prefix.as_deref() {
      Some(prefix) => {
        let Some((query_prefix, query_local)) = query.split_once(':') else {
          return false;
        };
        if is_html {
          prefix.eq_ignore_ascii_case(query_prefix) && self.local_name.eq_ignore_ascii_case(query_local)
        } else {
          prefix == query_prefix && self.local_name == query_local
        }
      }
      None => {
        if is_html {
          self.local_name.eq_ignore_ascii_case(query)
        } else {
          self.local_name == query
        }
      }
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
  Document {
    quirks_mode: QuirksMode,
  },
  /// A detached container for child nodes.
  ///
  /// Document fragments are never inserted directly into the tree: inserting a fragment moves its
  /// children into the target parent (in order) and empties the fragment.
  DocumentFragment,
  /// An HTML comment node.
  ///
  /// Comments are currently ignored when snapshotting back into the renderer's immutable `DomNode`
  /// representation, to match `crate::dom::parse_html` behavior.
  Comment {
    content: String,
  },
  /// An XML processing instruction node.
  ///
  /// Like comments, processing instructions are ignored by renderer snapshots.
  ProcessingInstruction {
    target: String,
    data: String,
  },
  /// A document type node.
  ///
  /// Note: the renderer DOM snapshot format (`crate::dom::DomNode`) currently drops doctypes (and
  /// comments), so `Document::to_renderer_dom` will not include these nodes.
  Doctype {
    name: String,
    public_id: String,
    system_id: String,
  },
  ShadowRoot {
    mode: ShadowRootMode,
    delegates_focus: bool,
    slot_assignment: SlotAssignmentMode,
    /// Whether this shadow root should be cloned when cloning its host element.
    ///
    /// Mirrors WHATWG DOM's `ShadowRoot.clonable` internal slot. Defaults to false.
    clonable: bool,
    /// Whether this shadow root is eligible for HTML serialization via declarative shadow DOM.
    ///
    /// Mirrors WHATWG DOM's `ShadowRoot.serializable` internal slot. Defaults to false.
    serializable: bool,
    /// Whether this shadow root originated from declarative shadow DOM markup.
    ///
    /// Mirrors WHATWG DOM's `ShadowRoot.declarative` internal slot. Defaults to false.
    declarative: bool,
  },
  Slot {
    namespace: String,
    attributes: Vec<Attribute>,
    assigned: bool,
  },
  Element {
    tag_name: String,
    namespace: String,
    prefix: Option<String>,
    attributes: Vec<Attribute>,
  },
  Text {
    content: String,
  },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Node {
  pub kind: NodeKind,
  pub parent: Option<NodeId>,
  pub children: Vec<NodeId>,
  /// Whether this node's children should be treated as inert for selector matching.
  ///
  /// This currently mirrors `crate::dom::DomNode::template_contents_are_inert()`, which represents
  /// inert template contents by keeping template descendants in `children` while skipping them for
  /// selector matching and other traversals.
  pub inert_subtree: bool,
  registered_observers: Vec<mutation_observer::RegisteredObserver>,
  pub script_already_started: bool,
  /// Whether this script element was created by the HTML parser.
  ///
  /// This mirrors the HTML spec's per-script-element "parser document" internal slot: it is set
  /// for parser-inserted scripts and cleared when the element stops being parser-inserted (e.g.
  /// when a parser-inserted script fails to run and may later be re-prepared dynamically).
  ///
  /// Fragment parsing (e.g. `innerHTML`) does **not** mark scripts as parser-inserted.
  pub script_parser_document: bool,
  /// Whether the script's `async` IDL attribute should default to true regardless of the presence
  /// of an explicit `async` content attribute.
  ///
  /// This mirrors the HTML spec's per-script-element "force async" flag:
  /// - Parser-inserted scripts set this to false.
  /// - Scripts created via DOM APIs default it to true.
  pub script_force_async: bool,
  pub mathml_annotation_xml_integration_point: bool,
}

impl Clone for Node {
  fn clone(&self) -> Self {
    Self {
      kind: self.kind.clone(),
      parent: self.parent,
      children: self.children.clone(),
      inert_subtree: self.inert_subtree,
      registered_observers: Vec::new(),
      script_already_started: self.script_already_started,
      script_parser_document: self.script_parser_document,
      script_force_async: self.script_force_async,
      mathml_annotation_xml_integration_point: self.mathml_annotation_xml_integration_point,
    }
  }
}

/// Summary of DOM mutations recorded since the last call to [`Document::take_mutations`].
///
/// This is used by `BrowserDocumentDom2` (and other hosts) to implement incremental invalidation
/// without requiring callers to manually classify changes.
#[derive(Debug, Default, Clone)]
pub(crate) struct MutationLog {
  /// Attribute names changed per node since the last `take_mutations()`.
  ///
  /// Names are normalized for HTML elements (ASCII-lowercased) so hosts can perform reliable
  /// comparisons (`"HREF"` and `"href"` are treated as the same attribute on HTML nodes).
  pub(crate) attribute_changed: FxHashMap<NodeId, FxHashSet<String>>,
  pub(crate) text_changed: FxHashSet<NodeId>,
  /// Parent nodes whose child list changed (insert/remove/reorder).
  pub(crate) child_list_changed: FxHashSet<NodeId>,
  /// Nodes that were inserted into a document-connected subtree.
  ///
  /// This is used by host-side incremental invalidation to precisely track damage from structural
  /// changes (for example, absolutely-positioned descendants that can paint outside their parent's
  /// border box).
  pub(crate) nodes_inserted: FxHashSet<NodeId>,
  /// Nodes that were removed from a document-connected subtree.
  ///
  /// Note: nodes are recorded before they become disconnected, so hosts can still map them back
  /// to previously computed layout/paint artifacts.
  pub(crate) nodes_removed: FxHashSet<NodeId>,
  /// Nodes whose live form control state changed (e.g. `<input>.value`, `<input>.checked`,
  /// `<textarea>.value`, `<option>.selected`).
  ///
  /// This is distinct from `attribute_changed` because these mutations must not be treated as style
  /// affecting content-attribute changes. Hosts can use this to trigger incremental repaint of
  /// form controls without falling back to full restyle/layout.
  pub(crate) form_state_changed: FxHashSet<NodeId>,
  /// Shadow DOM slot distribution / composed tree structure changed.
  ///
  /// This is distinct from `child_list_changed`: slot distribution can change without any DOM tree
  /// structural mutations (e.g. `HTMLSlotElement.assign(..)` or attribute changes that affect
  /// assignment). Hosts must treat this as a structural invalidation because the renderer's
  /// composed-tree snapshot and selector matching depend on slot assignment.
  pub(crate) composed_tree_changed: FxHashSet<NodeId>,
  /// Some render-affecting mutation occurred without structured classification.
  ///
  /// Hosts should conservatively fall back to a full pipeline run / fresh renderer-DOM snapshot when
  /// this is set, to avoid incremental fast-paths silently acknowledging out-of-band changes.
  pub(crate) unclassified: bool,
}

impl MutationLog {
  pub(crate) fn is_empty(&self) -> bool {
    self.attribute_changed.is_empty()
      && self.text_changed.is_empty()
      && self.child_list_changed.is_empty()
      && self.nodes_inserted.is_empty()
      && self.nodes_removed.is_empty()
      && self.form_state_changed.is_empty()
      && self.composed_tree_changed.is_empty()
      && !self.unclassified
  }

  pub(crate) fn clear(&mut self) {
    self.attribute_changed.clear();
    self.text_changed.clear();
    self.child_list_changed.clear();
    self.nodes_inserted.clear();
    self.nodes_removed.clear();
    self.form_state_changed.clear();
    self.composed_tree_changed.clear();
    self.unclassified = false;
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
  Html,
  Xml,
}

pub struct Document {
  kind: DocumentKind,
  nodes: Vec<Node>,
  // Form control state slots keyed by `NodeId` index.
  input_states: Vec<Option<form_controls::InputState>>,
  textarea_states: Vec<Option<form_controls::TextareaState>>,
  option_states: Vec<Option<form_controls::OptionState>>,
  root: NodeId,
  ready_state: DocumentReadyState,
  events: web_events::EventListenerRegistry,
  has_window_event_parent: bool,
  scripting_enabled: bool,
  mutations: MutationLog,
  mutation_generation: u64,
  selector_snapshot_cache: Option<SelectorSnapshotCache>,
  slotting: slotting::SlottingState,
  mutation_observer_agent: Rc<RefCell<mutation_observer::MutationObserverAgent>>,
  live_mutation: live_mutation::LiveMutation,
  intersection_observers: intersection_observer::IntersectionObserverRegistry,
  resize_observers: resize_observer::ResizeObserverRegistry,
  node_iterators: FxHashMap<NodeIteratorId, NodeIteratorState>,
  next_node_iterator_id: u64,
  ranges: FxHashMap<LiveRangeId, range::Range>,
}

impl Clone for Document {
  fn clone(&self) -> Self {
    Self {
      kind: self.kind,
      nodes: self.nodes.clone(),
      input_states: self.input_states.clone(),
      textarea_states: self.textarea_states.clone(),
      option_states: self.option_states.clone(),
      root: self.root,
      ready_state: self.ready_state,
      // Cloning a DOM tree should not implicitly clone active event listeners. Start with an empty
      // registry so callers can snapshot structure without inheriting the old event graph.
      events: web_events::EventListenerRegistry::new(),
      has_window_event_parent: self.has_window_event_parent,
      scripting_enabled: self.scripting_enabled,
      // Mutation logs are per-host derived state, not part of the DOM tree snapshot.
      mutations: MutationLog::default(),
      mutation_generation: self.mutation_generation,
      selector_snapshot_cache: None,
      slotting: self.slotting.clone(),
      mutation_observer_agent: Rc::new(RefCell::new(mutation_observer::MutationObserverAgent::new())),
      live_mutation: live_mutation::LiveMutation::default(),
      intersection_observers: intersection_observer::IntersectionObserverRegistry::new(self.nodes.len()),
      resize_observers: resize_observer::ResizeObserverRegistry::new(self.nodes.len()),
      node_iterators: FxHashMap::default(),
      next_node_iterator_id: 1,
      ranges: FxHashMap::default(),
    }
  }
}

impl std::fmt::Debug for Document {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Document")
      .field("kind", &self.kind)
      .field("nodes", &self.nodes)
      .field("root", &self.root)
      .field("scripting_enabled", &self.scripting_enabled)
      .field("mutation_generation", &self.mutation_generation)
      .finish()
  }
}

#[derive(Debug, Clone)]
pub struct RendererDomMapping {
  /// 1-based pre-order id (as produced by `crate::dom::enumerate_dom_ids`) -> `dom2` [`NodeId`].
  ///
  /// Index 0 is always `None` so renderer ids can be used directly as indexes.
  preorder_to_node_id: Vec<Option<NodeId>>,
  /// `dom2` [`NodeId`] index -> 1-based pre-order id.
  ///
  /// Uses 0 for nodes that are not reachable from the document root.
  node_id_to_preorder: Vec<usize>,
}

impl RendererDomMapping {
  /// Translate a 1-based renderer pre-order id (as produced by [`crate::dom::enumerate_dom_ids`])
  /// back into a `dom2` [`NodeId`].
  ///
  /// Note: the renderer snapshot may include synthetic nodes that do not exist in the `dom2` tree
  /// (currently: `<wbr>` synthesizes a zero-width break text node). For these nodes, the returned
  /// `NodeId` will be the corresponding real `dom2` node (e.g., the parent `<wbr>` element),
  /// meaning multiple renderer preorder ids can map to the same `NodeId`.
  pub fn node_id_for_preorder(&self, preorder_id: usize) -> Option<NodeId> {
    self.preorder_to_node_id.get(preorder_id).copied().flatten()
  }

  /// Translate a `dom2` [`NodeId`] to its 1-based renderer pre-order id.
  ///
  /// Returns `None` for nodes that are not reachable from the document root (detached subtrees). If
  /// a `dom2` node corresponds to multiple renderer preorder ids (e.g. `<wbr>` + its synthetic text
  /// child), this returns the preorder id of the real node (the `<wbr>` element) and not the
  /// synthetic one.
  pub fn preorder_for_node_id(&self, node_id: NodeId) -> Option<usize> {
    self
      .node_id_to_preorder
      .get(node_id.index())
      .copied()
      .and_then(|v| (v != 0).then_some(v))
  }

  /// Returns the full renderer-preorder → dom2-node mapping used by this snapshot.
  ///
  /// This includes entries for synthetic renderer nodes (currently the implicit ZWSP text child
  /// generated for `<wbr>`), which map back to their owning real dom2 node. Callers that need to
  /// validate preorder alignment across snapshots (e.g. before reusing node-id keyed caches) should
  /// compare this slice, not `node_id_to_preorder`.
  pub(crate) fn preorder_to_node_id(&self) -> &[Option<NodeId>] {
    &self.preorder_to_node_id
  }

  /// Compare two connected `dom2` [`NodeId`]s in renderer pre-order (document order).
  ///
  /// Returns `None` if either node is detached/unmappable (i.e. not reachable from the current
  /// document root).
  pub fn cmp_node_ids(&self, a: NodeId, b: NodeId) -> Option<std::cmp::Ordering> {
    Some(self.preorder_for_node_id(a)?.cmp(&self.preorder_for_node_id(b)?))
  }
}

#[derive(Debug, Clone)]
struct SelectorDomMapping {
  preorder_to_node_id: Vec<Option<NodeId>>,
  node_id_to_preorder: Vec<usize>,
}

impl SelectorDomMapping {
  pub fn node_id_for_preorder(&self, preorder_id: usize) -> Option<NodeId> {
    self.preorder_to_node_id.get(preorder_id).copied().flatten()
  }

  pub fn preorder_for_node_id(&self, node_id: NodeId) -> Option<usize> {
    self
      .node_id_to_preorder
      .get(node_id.index())
      .copied()
      .and_then(|v| (v != 0).then_some(v))
  }
}

#[derive(Debug, Clone)]
struct SelectorSnapshotCache {
  generation: u64,
  nodes_len: usize,
  scripting_enabled: bool,
  dom: Arc<DomNode>,
  mapping: Arc<SelectorDomMapping>,
}

pub struct RendererDomSnapshot {
  /// Immutable renderer DOM snapshot plus a `dom2` ↔ renderer preorder mapping.
  ///
  /// Mapping semantics:
  /// - Preorder ids are the same ids produced by [`crate::dom::enumerate_dom_ids`] over `dom`
  ///   (1-based, depth-first pre-order traversal).
  /// - This traversal includes inert `<template>` contents and declarative shadow roots, mirroring
  ///   the renderer's id scheme.
  /// - Some renderer nodes are synthetic (currently: the implicit ZWSP text child for HTML `<wbr>`);
  ///   these ids map back to the real owning `NodeId` (the `<wbr>` element), so `node_id_for_preorder`
  ///   is not necessarily 1:1.
  ///
  /// Note: selector/query APIs (`query_selector`, `matches_selector`, ...) use a separate internal
  /// snapshot mapping that follows selector traversal semantics (skipping inert template contents).
  pub dom: DomNode,
  pub mapping: RendererDomMapping,
}

impl Document {
  /// Legacy DOM Events factory (`Document.prototype.createEvent`).
  ///
  /// This is exposed primarily for compatibility with legacy scripts that still use
  /// `createEvent/initEvent/initCustomEvent`.
  pub fn create_event(&self, interface_name: &str) -> Result<web_events::Event, DomException> {
    let name = interface_name.trim();
    if name.eq_ignore_ascii_case("Event") {
      return Ok(web_events::Event::new("", web_events::EventInit::default()));
    }
    if name.eq_ignore_ascii_case("CustomEvent") {
      return Ok(web_events::Event::new_custom_event(
        "",
        web_events::CustomEventInit::default(),
      ));
    }
    Err(DomException::not_supported_error(format!(
      "Unsupported event interface: {name}"
    )))
  }

  /// Clone the document including the active event listener registry.
  ///
  /// `Document`'s `Clone` implementation intentionally resets the listener registry so callers can
  /// snapshot a tree's structure without implicitly inheriting active listeners. When an embedding
  /// needs to transfer the *live* document state (e.g. between a streaming HTML parser and a host),
  /// use this method instead.
  pub fn clone_with_events(&self) -> Self {
    Self {
      kind: self.kind,
      nodes: self.nodes.clone(),
      input_states: self.input_states.clone(),
      textarea_states: self.textarea_states.clone(),
      option_states: self.option_states.clone(),
      root: self.root,
      ready_state: self.ready_state,
      events: self.events.clone(),
      has_window_event_parent: self.has_window_event_parent,
      scripting_enabled: self.scripting_enabled,
      // Mutation logs are per-host derived state, not part of the DOM tree snapshot.
      mutations: MutationLog::default(),
      mutation_generation: self.mutation_generation,
      selector_snapshot_cache: None,
      slotting: self.slotting.clone(),
      mutation_observer_agent: Rc::new(RefCell::new(mutation_observer::MutationObserverAgent::new())),
      live_mutation: live_mutation::LiveMutation::default(),
      intersection_observers: intersection_observer::IntersectionObserverRegistry::new(self.nodes.len()),
      resize_observers: resize_observer::ResizeObserverRegistry::new(self.nodes.len()),
      node_iterators: FxHashMap::default(),
      next_node_iterator_id: 1,
      ranges: FxHashMap::default(),
    }
  }

  /// Convert a raw node index (e.g. from an FFI/binding handle) into a validated [`NodeId`].
  ///
  /// This is the preferred way for external code to "re-hydrate" a `NodeId` from an integer, since
  /// it guarantees the index is in-bounds for this `Document`.
  pub fn node_id_from_index(&self, index: usize) -> Result<NodeId, DomError> {
    if index < self.nodes.len() {
      Ok(NodeId(index))
    } else {
      Err(DomError::NotFoundError)
    }
  }

  fn should_inject_wbr_zwsp(&self, node_id: NodeId) -> bool {
    if !self.is_html_document() {
      return false;
    }
    let node = self.node(node_id);
    let NodeKind::Element {
      tag_name,
      namespace,
      ..
    } = &node.kind
    else {
      return false;
    };
    if !tag_name.eq_ignore_ascii_case("wbr") {
      return false;
    }
    if !self.is_html_case_insensitive_namespace(namespace) {
      return false;
    }

    // Avoid duplicating the renderer's historical `<wbr>` behaviour when importing from an
    // existing renderer DOM tree that may already contain a ZWSP text node child.
    //
    // Only consider children that are actually connected to the `<wbr>` element via a consistent
    // parent pointer; if the tree is partially detached, we still want to inject the synthetic
    // node for the connected renderer snapshot.
    for &child in &node.children {
      let Some(child_node) = self.nodes.get(child.index()) else {
        continue;
      };
      if child_node.parent != Some(node_id) {
        continue;
      }
      if let NodeKind::Text { content } = &child_node.kind {
        if content == "\u{200B}" {
          return false;
        }
      }
    }

    true
  }

  pub fn new_with_scripting(quirks_mode: QuirksMode, scripting_enabled: bool) -> Self {
    Self::new_with_mutation_observer_agent(
      quirks_mode,
      scripting_enabled,
      Rc::new(RefCell::new(mutation_observer::MutationObserverAgent::new())),
    )
  }

  pub fn new_with_mutation_observer_agent(
    quirks_mode: QuirksMode,
    scripting_enabled: bool,
    mutation_observer_agent: Rc<RefCell<mutation_observer::MutationObserverAgent>>,
  ) -> Self {
    let mut doc = Self {
      kind: DocumentKind::Html,
      nodes: Vec::new(),
      input_states: Vec::new(),
      textarea_states: Vec::new(),
      option_states: Vec::new(),
      root: NodeId(0),
      ready_state: DocumentReadyState::Loading,
      events: web_events::EventListenerRegistry::new(),
      has_window_event_parent: true,
      scripting_enabled,
      mutations: MutationLog::default(),
      mutation_generation: 0,
      selector_snapshot_cache: None,
      slotting: slotting::SlottingState::default(),
      mutation_observer_agent,
      live_mutation: live_mutation::LiveMutation::default(),
      intersection_observers: intersection_observer::IntersectionObserverRegistry::new(0),
      resize_observers: resize_observer::ResizeObserverRegistry::new(0),
      node_iterators: FxHashMap::default(),
      next_node_iterator_id: 1,
      ranges: FxHashMap::default(),
    };
    let root = doc.push_node(
      NodeKind::Document { quirks_mode },
      None,
      /* inert_subtree */ false,
    );
    debug_assert_eq!(root, NodeId(0));
    doc.root = root;
    doc
  }

  pub fn new(quirks_mode: QuirksMode) -> Self {
    Self::new_with_scripting(quirks_mode, true)
  }

  pub fn new_xml() -> Self {
    let mut doc = Self {
      kind: DocumentKind::Xml,
      nodes: Vec::new(),
      input_states: Vec::new(),
      textarea_states: Vec::new(),
      option_states: Vec::new(),
      root: NodeId(0),
      ready_state: DocumentReadyState::Loading,
      events: web_events::EventListenerRegistry::new(),
      // DOMParser "text/xml"/"application/xml"/... flavors return a Document without a browsing
      // context, so it does not have a Window event parent by default.
      has_window_event_parent: false,
      // DOMParser "text/xml"/"application/xml"/... flavors return a Document with scripting disabled.
      scripting_enabled: false,
      mutations: MutationLog::default(),
      mutation_generation: 0,
      selector_snapshot_cache: None,
      slotting: slotting::SlottingState::default(),
      mutation_observer_agent: Rc::new(RefCell::new(mutation_observer::MutationObserverAgent::new())),
      live_mutation: live_mutation::LiveMutation::default(),
      intersection_observers: intersection_observer::IntersectionObserverRegistry::new(0),
      resize_observers: resize_observer::ResizeObserverRegistry::new(0),
      node_iterators: FxHashMap::default(),
      next_node_iterator_id: 1,
      ranges: FxHashMap::default(),
    };
    let root = doc.push_node(
      NodeKind::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      None,
      /* inert_subtree */ false,
    );
    debug_assert_eq!(root, NodeId(0));
    doc.root = root;
    doc
  }

  pub fn is_html_document(&self) -> bool {
    matches!(self.kind, DocumentKind::Html)
  }

  pub fn is_html_case_insensitive_namespace(&self, ns: &str) -> bool {
    self.is_html_document() && (ns.is_empty() || ns == HTML_NAMESPACE)
  }

  fn kind_implies_inert_subtree(&self, kind: &NodeKind) -> bool {
    match kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        tag_name.eq_ignore_ascii_case("template")
          && self.is_html_case_insensitive_namespace(namespace)
      }
      _ => false,
    }
  }

  fn kind_is_html_script(&self, kind: &NodeKind) -> bool {
    match kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        tag_name.eq_ignore_ascii_case("script")
          && self.is_html_case_insensitive_namespace(namespace)
      }
      _ => false,
    }
  }

  pub fn root(&self) -> NodeId {
    self.root
  }

  /// Monotonic counter incremented on DOM mutations that can affect rendering.
  ///
  /// This is used by embedding layers (e.g. `BrowserDocumentDom2`) to detect when the live `dom2`
  /// document has changed even if the mutation bypassed higher-level invalidation hooks (such as
  /// raw-pointer JS shims).
  pub fn mutation_generation(&self) -> u64 {
    self.mutation_generation
  }

  #[inline]
  fn bump_mutation_generation_classified(&mut self) {
    self.mutation_generation = self.mutation_generation.wrapping_add(1);
    // Selector/query APIs build a renderer-style snapshot for matching; invalidate it eagerly on any
    // render-affecting mutation so subsequent queries observe the updated tree and we don't retain
    // multiple generations of large snapshots.
    self.selector_snapshot_cache = None;
  }

  #[inline]
  fn bump_mutation_generation_unclassified(&mut self) {
    self.mutation_generation = self.mutation_generation.wrapping_add(1);
    // Selector/query APIs build a renderer-style snapshot for matching; invalidate it eagerly on any
    // render-affecting mutation so subsequent queries observe the updated tree and we don't retain
    // multiple generations of large snapshots.
    self.selector_snapshot_cache = None;
    self.mutations.unclassified = true;
  }

  pub fn ready_state(&self) -> DocumentReadyState {
    self.ready_state
  }

  pub fn set_ready_state(&mut self, state: DocumentReadyState) {
    self.ready_state = state;
  }

  pub fn events(&self) -> &web_events::EventListenerRegistry {
    &self.events
  }

  pub fn events_mut(&mut self) -> &mut web_events::EventListenerRegistry {
    &mut self.events
  }

  pub fn has_window_event_parent(&self) -> bool {
    self.has_window_event_parent
  }

  pub fn set_has_window_event_parent(&mut self, has: bool) {
    self.has_window_event_parent = has;
  }
  pub fn node(&self, id: NodeId) -> &Node {
    &self.nodes[id.0]
  }

  pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
    // `node_mut` allows callers to mutate `Node` state directly, bypassing higher-level mutation APIs
    // that record structured invalidation data. Conservatively bump the mutation generation so hosts
    // can still detect out-of-band DOM changes (e.g. raw-pointer JS shims).
    self.bump_mutation_generation_unclassified();
    &mut self.nodes[id.0]
  }

  pub fn create_node_iterator(&mut self, root: NodeId) -> NodeIteratorId {
    let id = NodeIteratorId::from_u64(self.next_node_iterator_id);
    self.next_node_iterator_id = self.next_node_iterator_id.wrapping_add(1);
    self.node_iterators.insert(
      id,
      NodeIteratorState {
        root,
        reference: root,
        pointer_before_reference: true,
      },
    );
    id
  }

  pub fn node_iterator_root(&self, id: NodeIteratorId) -> Option<NodeId> {
    self.node_iterators.get(&id).map(|state| state.root)
  }

  pub fn node_iterator_reference(&self, id: NodeIteratorId) -> Option<NodeId> {
    self.node_iterators.get(&id).map(|state| state.reference)
  }

  pub fn node_iterator_pointer_before_reference(&self, id: NodeIteratorId) -> Option<bool> {
    self
      .node_iterators
      .get(&id)
      .map(|state| state.pointer_before_reference)
  }

  pub fn set_node_iterator_reference_and_pointer(
    &mut self,
    id: NodeIteratorId,
    reference: NodeId,
    pointer_before_reference: bool,
  ) {
    let Some(state) = self.node_iterators.get_mut(&id) else {
      return;
    };
    state.reference = reference;
    state.pointer_before_reference = pointer_before_reference;
  }

  pub fn remove_node_iterator(&mut self, id: NodeIteratorId) {
    self.node_iterators.remove(&id);
  }

  /// Spec: <https://dom.spec.whatwg.org/#nodeiterator-pre-removing-steps>
  pub(crate) fn node_iterator_pre_remove_steps(&mut self, to_be_removed: NodeId) {
    if self.node_iterators.is_empty() {
      return;
    }

    let last_descendant = self.tree_last_inclusive_descendant(to_be_removed);
    let mut updates: Vec<(NodeIteratorId, NodeId, bool)> = Vec::new();

    for (&id, state) in &self.node_iterators {
      if state.root == to_be_removed {
        continue;
      }

      if !self.is_tree_inclusive_ancestor(to_be_removed, state.reference) {
        continue;
      }

      let mut pointer_before_reference = state.pointer_before_reference;
      if pointer_before_reference {
        if let Some(next) = self.tree_following_in_subtree(state.root, last_descendant) {
          updates.push((id, next, pointer_before_reference));
          continue;
        }
        pointer_before_reference = false;
      }

      let reference = if let Some(previous_sibling) = self.tree_previous_sibling(to_be_removed) {
        self.tree_last_inclusive_descendant(previous_sibling)
      } else {
        self.tree_parent_node(to_be_removed).unwrap_or(state.root)
      };

      updates.push((id, reference, pointer_before_reference));
    }

    for (id, reference, pointer_before_reference) in updates {
      let Some(state) = self.node_iterators.get_mut(&id) else {
        continue;
      };
      state.reference = reference;
      state.pointer_before_reference = pointer_before_reference;
    }
  }

  pub fn script_already_started(&self, node: NodeId) -> Result<bool, DomError> {
    let node = self.node_checked(node)?;
    if !self.kind_is_html_script(&node.kind) {
      return Err(DomError::InvalidNodeTypeError);
    }
    Ok(node.script_already_started)
  }

  pub fn set_script_already_started(&mut self, node: NodeId, value: bool) -> Result<(), DomError> {
    // This is a per-script-element internal slot that does not affect rendering. Avoid bumping the
    // mutation generation so hosts can use it to detect *real* DOM changes.
    let is_script = {
      let node = self.node_checked(node)?;
      self.kind_is_html_script(&node.kind)
    };
    if !is_script {
      return Err(DomError::InvalidNodeTypeError);
    }
    let node = self.node_checked_mut(node)?;
    node.script_already_started = value;
    Ok(())
  }

  pub fn script_force_async(&self, node: NodeId) -> Result<bool, DomError> {
    let node = self.node_checked(node)?;
    if !self.kind_is_html_script(&node.kind) {
      return Err(DomError::InvalidNodeTypeError);
    }
    Ok(node.script_force_async)
  }

  pub fn set_script_force_async(&mut self, node: NodeId, value: bool) -> Result<(), DomError> {
    // This is a per-script-element internal slot that does not affect rendering. Avoid bumping the
    // mutation generation so hosts can use it to detect *real* DOM changes.
    let is_script = {
      let node = self.node_checked(node)?;
      self.kind_is_html_script(&node.kind)
    };
    if !is_script {
      return Err(DomError::InvalidNodeTypeError);
    }
    let node = self.node_checked_mut(node)?;
    node.script_force_async = value;
    Ok(())
  }

  pub fn script_parser_document(&self, node: NodeId) -> Result<bool, DomError> {
    let node = self.node_checked(node)?;
    if !self.kind_is_html_script(&node.kind) {
      return Err(DomError::InvalidNodeTypeError);
    }
    Ok(node.script_parser_document)
  }
  pub fn set_script_parser_document(&mut self, node: NodeId, value: bool) -> Result<(), DomError> {
    // This is a per-script-element internal slot that does not affect rendering. Avoid bumping the
    // mutation generation so hosts can use it to detect *real* DOM changes.
    let is_script = {
      let node = self.node_checked(node)?;
      self.kind_is_html_script(&node.kind)
    };
    if !is_script {
      return Err(DomError::InvalidNodeTypeError);
    }
    let node = self.node_checked_mut(node)?;
    node.script_parser_document = value;
    Ok(())
  }

  /// HTML: "prepare a script" steps 2–4 (internal-slot reset).
  ///
  /// These steps must run even when the element is not eligible for execution (e.g. scripts inside
  /// inert `<template>` contents), because they transition the element out of the parser-inserted
  /// state so later DOM mutations/insertion treat it like a dynamic script element.
  pub(crate) fn reset_parser_inserted_script_internal_slots(&mut self, node: NodeId) {
    let was_parser_inserted = match self.script_parser_document(node) {
      Ok(value) => value,
      Err(_) => return,
    };

    self
      .set_script_parser_document(node, false)
      .expect("set_script_parser_document should succeed for <script>"); // fastrender-allow-unwrap

    if was_parser_inserted && !self.has_attribute(node, "async").unwrap_or(false) {
      self
        .set_script_force_async(node, true)
        .expect("set_script_force_async should succeed for <script>"); // fastrender-allow-unwrap
    }
  }

  pub fn nodes(&self) -> &[Node] {
    &self.nodes
  }

  pub fn nodes_len(&self) -> usize {
    self.nodes.len()
  }

  /// Take (and clear) the accumulated mutation log.
  pub(crate) fn take_mutations(&mut self) -> MutationLog {
    std::mem::take(&mut self.mutations)
  }

  pub(crate) fn set_live_mutation_hook(
    &mut self,
    hook: Option<Box<dyn live_mutation::LiveMutationHook>>,
  ) {
    self.live_mutation.set_hook(hook);
  }

  /// Clear any accumulated mutation records.
  pub(crate) fn clear_mutations(&mut self) {
    self.mutations.clear();
  }

  #[inline]
  fn record_attribute_mutation(&mut self, node: NodeId, name: &str) {
    // Attribute names on HTML elements are ASCII-case-insensitive. Normalize to a stable lowercase
    // form so hosts can reliably match on `"href"`, `"class"`, etc.
    let is_html = match &self.node(node).kind {
      NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => {
        self.is_html_case_insensitive_namespace(namespace)
      }
      _ => true,
    };
    let normalized = if is_html {
      name.to_ascii_lowercase()
    } else {
      name.to_string()
    };
    self
      .mutations
      .attribute_changed
      .entry(node)
      .or_default()
      .insert(normalized);
  }

  #[inline]
  fn record_text_mutation(&mut self, node: NodeId) {
    self.mutations.text_changed.insert(node);
  }

  #[inline]
  fn record_child_list_mutation(&mut self, parent: NodeId) {
    self.mutations.child_list_changed.insert(parent);
  }

  #[inline]
  fn record_node_inserted(&mut self, node: NodeId) {
    if self.is_connected_for_scripting(node) {
      self.mutations.nodes_inserted.insert(node);
    }
  }

  #[inline]
  fn record_node_removed(&mut self, node: NodeId) {
    if self.is_connected_for_scripting(node) {
      self.mutations.nodes_removed.insert(node);
    }
  }

  #[inline]
  fn record_form_state_mutation(&mut self, node: NodeId) {
    self.mutations.form_state_changed.insert(node);
  }

  #[inline]
  fn record_composed_tree_mutation(&mut self, node: NodeId) {
    self.mutations.composed_tree_changed.insert(node);
  }

  fn node_checked(&self, id: NodeId) -> Result<&Node, DomError> {
    self.nodes.get(id.index()).ok_or(DomError::NotFoundError)
  }

  fn node_checked_mut(&mut self, id: NodeId) -> Result<&mut Node, DomError> {
    self
      .nodes
      .get_mut(id.index())
      .ok_or(DomError::NotFoundError)
  }

  /// Returns the document element.
  ///
  /// This is the first child of the document root that is an element (including `<slot>`),
  /// in tree order.
  pub fn document_element_for(&self, document: NodeId) -> Option<NodeId> {
    let root_node = self.nodes.get(document.index())?;
    root_node.children.iter().copied().find(|&child| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| node.parent == Some(document))
        && matches!(
          self.nodes[child.index()].kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
    })
  }

  /// Returns the document type node, if any.
  ///
  /// This is the first child of the document root that is a doctype node, in tree order.
  pub fn doctype_for(&self, document: NodeId) -> Option<NodeId> {
    let root_node = self.nodes.get(document.index())?;
    root_node.children.iter().copied().find(|&child| {
      self.nodes.get(child.index()).is_some_and(|node| {
        node.parent == Some(document) && matches!(&node.kind, NodeKind::Doctype { .. })
      })
    })
  }

  /// Returns the document type node for the primary document, if any.
  pub fn doctype(&self) -> Option<NodeId> {
    self.doctype_for(self.root())
  }

  pub fn document_element(&self) -> Option<NodeId> {
    self.document_element_for(self.root())
  }

  /// Returns the first element in tree order whose `id` attribute matches `id`.
  ///
  /// This query:
  /// - returns `None` for an empty `id`,
  /// - ignores detached subtrees, and
  /// - ignores nodes inside inert `<template>` contents (`Node::inert_subtree`).
  ///
  /// Shadow-root semantics:
  /// - `Document.getElementById` does not traverse into shadow roots.
  /// - `ShadowRoot.getElementById` traverses its own tree scope, but does not pierce into nested
  ///   shadow roots.
  pub fn get_element_by_id_from(&self, root: NodeId, id: &str) -> Option<NodeId> {
    if id.is_empty() {
      return None;
    }

    let allow_root_shadow = self
      .nodes
      .get(root.index())
      .is_some_and(|node| matches!(&node.kind, NodeKind::ShadowRoot { .. }));
    let mut remaining = self.nodes.len() + 1;
    let mut stack: Vec<NodeId> = vec![root];
    while let Some(node_id) = stack.pop() {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      let Some(node) = self.nodes.get(node_id.index()) else {
        continue;
      };

      if let NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } = &node.kind
      {
        let is_html = self.is_html_case_insensitive_namespace(namespace);
        if attributes.iter().any(|attr| {
          attr.namespace == NULL_NAMESPACE
            && (if is_html {
              attr.local_name.eq_ignore_ascii_case("id")
            } else {
              attr.local_name == "id"
            })
            && attr.value == id
        }) {
          return Some(node_id);
        }
      }

      if node.inert_subtree {
        continue;
      }
      if matches!(&node.kind, NodeKind::ShadowRoot { .. })
        && !(allow_root_shadow && node_id == root)
      {
        continue;
      }

      for &child in node.children.iter().rev() {
        let Some(child_node) = self.nodes.get(child.index()) else {
          continue;
        };
        if child_node.parent != Some(node_id) {
          continue;
        }
        if matches!(&child_node.kind, NodeKind::ShadowRoot { .. }) {
          continue;
        }
        stack.push(child);
      }
    }

    None
  }

  pub fn get_element_by_id(&self, id: &str) -> Option<NodeId> {
    self.get_element_by_id_from(self.root(), id)
  }

  #[inline]
  fn is_html_element(&self, node_id: NodeId, tag: &str) -> bool {
    if !self.is_html_document() {
      return false;
    }
    let Some(node) = self.nodes.get(node_id.index()) else {
      return false;
    };
    match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if self.is_html_case_insensitive_namespace(namespace)
        && tag_name.eq_ignore_ascii_case(tag) =>
      {
        true
      }
      _ => false,
    }
  }

  /// Returns the document's HTML `<head>` element, if any.
  ///
  /// Minimal HTML-ish semantics:
  /// - If `documentElement` exists and is an HTML `<html>` element, return the first HTML `<head>`
  ///   child element (tree order).
  /// - Otherwise return `None`.
  pub fn head_for(&self, document: NodeId) -> Option<NodeId> {
    let html = self.document_element_for(document)?;
    if !self.is_html_element(html, "html") {
      return None;
    }
    let html_node = self.nodes.get(html.index())?;
    html_node.children.iter().copied().find(|&child| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| node.parent == Some(html))
        && self.is_html_element(child, "head")
    })
  }

  pub fn head(&self) -> Option<NodeId> {
    self.head_for(self.root())
  }

  /// Returns the document's HTML `<body>` element, if any.
  ///
  /// Minimal HTML-ish semantics:
  /// - If `documentElement` exists and is an HTML `<html>` element, return the first HTML `<body>`
  ///   child element (tree order).
  /// - If no `<body>` is present, return the first HTML `<frameset>` child element (tree order).
  /// - Otherwise return `None`.
  pub fn body_for(&self, document: NodeId) -> Option<NodeId> {
    let html = self.document_element_for(document)?;
    if !self.is_html_element(html, "html") {
      return None;
    }
    let html_node = self.nodes.get(html.index())?;
    let is_direct_child = |child: NodeId| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| node.parent == Some(html))
    };

    if let Some(body) = html_node
      .children
      .iter()
      .copied()
      .find(|&child| is_direct_child(child) && self.is_html_element(child, "body"))
    {
      return Some(body);
    }

    html_node
      .children
      .iter()
      .copied()
      .find(|&child| is_direct_child(child) && self.is_html_element(child, "frameset"))
  }

  pub fn body(&self) -> Option<NodeId> {
    self.body_for(self.root())
  }

  fn push_node(&mut self, kind: NodeKind, parent: Option<NodeId>, inert_subtree: bool) -> NodeId {
    let id = NodeId(self.nodes.len());
    let inert_subtree = inert_subtree || self.kind_implies_inert_subtree(&kind);
    let (input_state, textarea_state, option_state) = self.init_form_control_state_for_node_kind(&kind);
    self.nodes.push(Node {
      kind,
      parent,
      children: Vec::new(),
      inert_subtree,
      registered_observers: Vec::new(),
      script_parser_document: false,
      script_already_started: false,
      script_force_async: false,
      mathml_annotation_xml_integration_point: false,
    });
    self.input_states.push(input_state);
    self.textarea_states.push(textarea_state);
    self.option_states.push(option_state);
    self.intersection_observers.on_node_added();
    self.resize_observers.on_node_added();
    if let Some(parent_id) = parent {
      self.nodes[parent_id.0].children.push(id);
    }
    id
  }

  /// Snapshot this `dom2` document back into the renderer's immutable [`DomNode`] representation.
  ///
  /// This is used for tests and incremental adoption (e.g. import into `dom2`, mutate, then render
  /// via existing code that consumes `DomNode`).
  fn snapshot_element_attributes(
    &self,
    node_id: NodeId,
    tag_name: &str,
    namespace: &str,
    attributes: &[Attribute],
  ) -> Vec<(String, String)> {
    fn trim_ascii_whitespace_html(value: &str) -> &str {
      // HTML attribute parsing ignores leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but
      // does not treat all Unicode whitespace as ignorable (e.g. NBSP).
      value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
    }

    fn attr_value_ci<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
      attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
    }

    fn remove_attr_ci(attrs: &mut Vec<(String, String)>, name: &str) {
      attrs.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    fn upsert_attr_ci(attrs: &mut Vec<(String, String)>, name: &str, value: String) {
      if let Some((_, v)) = attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
        *v = value;
      } else {
        attrs.push((name.to_string(), value));
      }
    }

    fn ensure_bool_attr_ci(attrs: &mut Vec<(String, String)>, name: &str, present: bool) {
      if present {
        if !attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
          attrs.push((name.to_string(), String::new()));
        }
      } else {
        remove_attr_ci(attrs, name);
      }
    }

    fn is_input_checkable(type_attr: Option<&str>) -> bool {
      let ty = type_attr.unwrap_or("text");
      ty.eq_ignore_ascii_case("checkbox") || ty.eq_ignore_ascii_case("radio")
    }

    let mut attrs: Vec<(String, String)> = attributes
      .iter()
      .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
      .collect();

    if !self.is_html_case_insensitive_namespace(namespace) {
      return attrs;
    }

    // NOTE: For HTML form controls, we intentionally project *runtime state* (e.g. `.value`,
    // `.checked`, `.selected`) into the snapshot attributes so style/layout/paint and selector
    // matching can observe JS-driven state updates without mutating author-visible DOM attributes.
    //
    // Semantic caveat: because we project current state into content attributes (e.g. `checked`,
    // `selected`, `value`), attribute selectors like `[checked]` and pseudo-classes like `:default`
    // currently behave as if they match the *current* state rather than the authored markup.

    if tag_name.eq_ignore_ascii_case("input") {
      let Some(state) = self
        .input_states
        .get(node_id.index())
        .and_then(|state| state.as_ref())
      else {
        return attrs;
      };

      let input_type = attr_value_ci(&attrs, "type")
        .map(trim_ascii_whitespace_html)
        .unwrap_or("text");
      let is_file_input = input_type.eq_ignore_ascii_case("file");
      let is_checkable = is_input_checkable(Some(input_type));
      if is_file_input {
        // File inputs never expose pre-filled value strings from markup.
        remove_attr_ci(&mut attrs, "value");
      } else {
        // Mirror the input's *current value* into the snapshot attribute.
        upsert_attr_ci(&mut attrs, "value", state.value.clone());
      }

      if is_checkable {
        ensure_bool_attr_ci(&mut attrs, "checked", state.checkedness);
      } else {
        // Ensure any stale authored `checked` doesn't leak into snapshots for non-checkable inputs.
        remove_attr_ci(&mut attrs, "checked");
      }
    } else if tag_name.eq_ignore_ascii_case("textarea") {
      let Some(state) = self
        .textarea_states
        .get(node_id.index())
        .and_then(|state| state.as_ref())
      else {
        return attrs;
      };

      // Mirror the textarea's *current value* using the renderer's dynamic override attribute,
      // matching `crate::dom::textarea_current_value` semantics.
      if state.dirty_value {
        upsert_attr_ci(&mut attrs, "data-fastr-value", state.value.clone());
      } else {
        remove_attr_ci(&mut attrs, "data-fastr-value");
      }
    } else if tag_name.eq_ignore_ascii_case("option") {
      let Some(state) = self
        .option_states
        .get(node_id.index())
        .and_then(|state| state.as_ref())
      else {
        return attrs;
      };

      ensure_bool_attr_ci(&mut attrs, "selected", state.selectedness);
    }

    attrs
  }

  pub fn to_renderer_dom(&self) -> DomNode {
    struct Frame {
      src: NodeId,
      dst: *mut DomNode,
      next_child: usize,
    }

    let scripting_enabled = self.scripting_enabled;
    let is_html_document = self.is_html_document();
    fn node_kind_to_dom_node_type(
      doc: &Document,
      node_id: NodeId,
      kind: &NodeKind,
      scripting_enabled: bool,
      is_html_document: bool,
    ) -> Option<DomNodeType> {
      Some(match kind {
        NodeKind::Document { quirks_mode } => DomNodeType::Document {
          quirks_mode: *quirks_mode,
          scripting_enabled,
          is_html_document,
        },
        NodeKind::DocumentFragment => {
          // The renderer DOM snapshot format does not have a first-class DocumentFragment node
          // type. Fragments should never be connected (insertion moves their children and leaves the
          // fragment detached), but map them defensively to a plain document node to avoid panics if
          // an invalid tree is constructed.
          DomNodeType::Document {
            quirks_mode: QuirksMode::NoQuirks,
            scripting_enabled,
            is_html_document,
          }
        }
        NodeKind::ShadowRoot {
          mode,
          delegates_focus,
          ..
        } => DomNodeType::ShadowRoot {
          mode: *mode,
          delegates_focus: *delegates_focus,
        },
        NodeKind::Slot {
          namespace,
          attributes,
          assigned,
        } => DomNodeType::Slot {
          namespace: namespace.clone(),
          attributes: attributes
            .iter()
            .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
            .collect(),
          assigned: *assigned,
        },
        NodeKind::Element {
          tag_name,
          namespace,
          prefix: _,
          attributes,
        } => DomNodeType::Element {
          tag_name: tag_name.clone(),
          namespace: namespace.clone(),
          attributes: doc.snapshot_element_attributes(node_id, tag_name, namespace, attributes),
        },
        NodeKind::Text { content } => DomNodeType::Text {
          content: content.clone(),
        },
        // html5ever-only node kinds that the renderer DOM representation currently drops.
        NodeKind::Comment { .. }
        | NodeKind::ProcessingInstruction { .. }
        | NodeKind::Doctype { .. } => return None,
      })
    }

    let root_id = self.root;
    let root_src = self.node(root_id);
    let mut root = DomNode {
      node_type: node_kind_to_dom_node_type(self, root_id, &root_src.kind, scripting_enabled, is_html_document)
        .unwrap_or_else(
        || {
          debug_assert!(
            false,
            "document root must be representable in renderer DOM snapshot"
          );
          DomNodeType::Document {
            quirks_mode: QuirksMode::NoQuirks,
            scripting_enabled,
            is_html_document,
          }
        },
      ),
      children: Vec::with_capacity(root_src.children.len()),
    };

    let mut stack: Vec<Frame> = vec![Frame {
      src: root_id,
      dst: &mut root as *mut DomNode,
      next_child: 0,
    }];

    while let Some(mut frame) = stack.pop() {
      let src = self.node(frame.src);
      // Safety: `dst` always points into `root` (the output tree). We only mutate the children vec
      // of a node after pushing its frame, and we never move nodes out of the output tree.
      let dst = unsafe { &mut *frame.dst };

      if frame.next_child < src.children.len() {
        let child_id = src.children[frame.next_child];
        frame.next_child += 1;
        let parent_id = frame.src;
        stack.push(frame);

        // Follow only consistent parent pointers so partially-detached nodes don't reappear in the
        // renderer snapshot.
        let child_src = self.node(child_id);
        if child_src.parent != Some(parent_id) {
          continue;
        }

        let Some(child_node_type) =
          node_kind_to_dom_node_type(self, child_id, &child_src.kind, scripting_enabled, is_html_document)
        else {
          continue;
        };

        let extra_capacity = usize::from(self.should_inject_wbr_zwsp(child_id));
        dst.children.push(DomNode {
          node_type: child_node_type,
          children: Vec::with_capacity(child_src.children.len() + extra_capacity),
        });
        let Some(child_dst) = dst.children.last_mut() else {
          debug_assert!(false, "child node missing after push");
          continue;
        };
        let child_dst = child_dst as *mut DomNode;
        stack.push(Frame {
          src: child_id,
          dst: child_dst,
          next_child: 0,
        });
      } else if self.should_inject_wbr_zwsp(frame.src) {
        // HTML <wbr> elements represent optional break opportunities. Synthesize a zero-width break
        // text node so line breaking can consider the opportunity while still allowing the element
        // to be styled/selected.
        dst.children.push(DomNode {
          node_type: DomNodeType::Text {
            content: "\u{200B}".to_string(),
          },
          children: Vec::new(),
        });
      }
    }

    root
  }

  fn to_renderer_dom_subtree(&self, root_id: NodeId) -> Option<DomNode> {
    struct Frame {
      src: NodeId,
      dst: *mut DomNode,
      next_child: usize,
    }

    let scripting_enabled = self.scripting_enabled;
    let is_html_document = self.is_html_document();
    fn node_kind_to_dom_node_type(
      doc: &Document,
      node_id: NodeId,
      kind: &NodeKind,
      scripting_enabled: bool,
      is_html_document: bool,
    ) -> Option<DomNodeType> {
      Some(match kind {
        NodeKind::Document { quirks_mode } => DomNodeType::Document {
          quirks_mode: *quirks_mode,
          scripting_enabled,
          is_html_document,
        },
        NodeKind::DocumentFragment => DomNodeType::Element {
          // `DomNodeType` has no first-class DocumentFragment representation. For subtree selector
          // queries we snapshot fragments as synthetic elements so `:scope` can anchor on the
          // fragment root, but we ensure selector mappings never return the synthetic root node.
          tag_name: "#document-fragment".to_string(),
          namespace: String::new(),
          attributes: Vec::new(),
        },
        NodeKind::ShadowRoot {
          mode,
          delegates_focus,
          ..
        } => DomNodeType::ShadowRoot {
          mode: *mode,
          delegates_focus: *delegates_focus,
        },
        NodeKind::Slot {
          namespace,
          attributes,
          assigned,
        } => DomNodeType::Slot {
          namespace: namespace.clone(),
          attributes: attributes
            .iter()
            .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
            .collect(),
          assigned: *assigned,
        },
        NodeKind::Element {
          tag_name,
          namespace,
          prefix: _,
          attributes,
        } => DomNodeType::Element {
          tag_name: tag_name.clone(),
          namespace: namespace.clone(),
          attributes: doc.snapshot_element_attributes(node_id, tag_name, namespace, attributes),
        },
        NodeKind::Text { content } => DomNodeType::Text {
          content: content.clone(),
        },
        // html5ever-only node kinds that the renderer DOM representation currently drops.
        NodeKind::Comment { .. }
        | NodeKind::ProcessingInstruction { .. }
        | NodeKind::Doctype { .. } => return None,
      })
    }

    let root_src = self.nodes.get(root_id.index())?;
    let fragment_root = matches!(root_src.kind, NodeKind::DocumentFragment);
    let root_type =
      node_kind_to_dom_node_type(self, root_id, &root_src.kind, scripting_enabled, is_html_document)?;
    let mut root = DomNode {
      node_type: root_type,
      children: Vec::with_capacity(root_src.children.len()),
    };

    let mut stack: Vec<Frame> = vec![Frame {
      src: root_id,
      dst: &mut root as *mut DomNode,
      next_child: 0,
    }];

    while let Some(mut frame) = stack.pop() {
      let src = self.node(frame.src);
      // Safety: `dst` always points into `root` (the output tree). We only mutate the children vec
      // of a node after pushing its frame, and we never move nodes out of the output tree.
      let dst = unsafe { &mut *frame.dst };

      if frame.next_child < src.children.len() {
        let child_id = src.children[frame.next_child];
        frame.next_child += 1;
        let parent_id = frame.src;
        stack.push(frame);

        let child_src = self.node(child_id);
        // Follow only consistent parent pointers so partially-detached nodes don't reappear in the
        // renderer snapshot.
        if child_src.parent != Some(parent_id) {
          continue;
        }
        let Some(child_node_type) =
          node_kind_to_dom_node_type(self, child_id, &child_src.kind, scripting_enabled, is_html_document)
        else {
          continue;
        };
        let extra_capacity = usize::from(self.should_inject_wbr_zwsp(child_id));
        dst.children.push(DomNode {
          node_type: child_node_type,
          children: Vec::with_capacity(child_src.children.len() + extra_capacity),
        });
        let Some(child_dst) = dst.children.last_mut().map(|node| node as *mut DomNode) else {
          debug_assert!(false, "child node missing after push");
          continue;
        };
        stack.push(Frame {
          src: child_id,
          dst: child_dst,
          next_child: 0,
        });
      } else if self.should_inject_wbr_zwsp(frame.src) {
        dst.children.push(DomNode {
          node_type: DomNodeType::Text {
            content: "\u{200B}".to_string(),
          },
          children: Vec::new(),
        });
      }
    }

    if fragment_root {
      // `DocumentFragment` querySelector(All) uses a "virtual scoping root" for selector matching
      // (Selectors 4). In the renderer snapshot we represent that scoping root as a synthetic
      // element so `:scope` can be anchored, but we do not map it back to a `NodeId` (see
      // `build_selector_preorder_mapping_from`).
      let children = std::mem::take(&mut root.children);
      return Some(DomNode {
        node_type: DomNodeType::Element {
          tag_name: "#document-fragment".to_string(),
          namespace: String::new(),
          attributes: Vec::new(),
        },
        children,
      });
    }

    Some(root)
  }

  /// Build a mapping between renderer preorder ids (as produced by `crate::dom::enumerate_dom_ids`)
  /// and `dom2` [`NodeId`]s for this document.
  ///
  /// This operation is **O(tree size)**. Callers that need to translate many renderer preorder ids
  /// (for example: high-frequency UI event dispatch) should cache the returned
  /// [`RendererDomMapping`] and invalidate it when [`Document::mutation_generation`] changes.
  pub fn build_renderer_preorder_mapping(&self) -> RendererDomMapping {
    // Preorder ids are 1-based (index 0 unused), matching `crate::dom::enumerate_dom_ids` and the
    // debug inspector.
    let mut preorder_to_node_id: Vec<Option<NodeId>> = Vec::with_capacity(self.nodes.len() + 1);
    preorder_to_node_id.push(None);
    let mut node_id_to_preorder: Vec<usize> = vec![0; self.nodes.len()];
    enum StackItem {
      Real(NodeId),
      SyntheticWbrZwsp(NodeId),
    }

    fn node_is_renderable(kind: &NodeKind) -> bool {
      !matches!(
        kind,
        NodeKind::Comment { .. }
          | NodeKind::ProcessingInstruction { .. }
          | NodeKind::Doctype { .. }
      )
    }

    let mut stack: Vec<StackItem> = vec![StackItem::Real(self.root)];
    while let Some(item) = stack.pop() {
      match item {
        StackItem::Real(id) => {
          let node = self.node(id);
          if !node_is_renderable(&node.kind) {
            continue;
          }

          let preorder_id = preorder_to_node_id.len();
          preorder_to_node_id.push(Some(id));
          node_id_to_preorder[id.0] = preorder_id;

          // Push children in reverse so we traverse in tree order.
          //
          // For `<wbr>` we also synthesize a trailing ZWSP text child in the renderer snapshot;
          // insert a synthetic stack item so preorder ids stay aligned.
          if self.should_inject_wbr_zwsp(id) {
            stack.push(StackItem::SyntheticWbrZwsp(id));
          }
          for &child in node.children.iter().rev() {
            let Some(child_node) = self.nodes.get(child.index()) else {
              continue;
            };
            if child_node.parent == Some(id) {
              stack.push(StackItem::Real(child));
            }
          }
        }
        StackItem::SyntheticWbrZwsp(parent) => {
          // Synthetic ZWSP nodes map back to their parent `<wbr>` element `NodeId`.
          preorder_to_node_id.push(Some(parent));
          // Do not overwrite `node_id_to_preorder` for the `<wbr>` element; it should remain the
          // preorder id of the element itself (the first mapping entry).
        }
      }
    }

    RendererDomMapping {
      preorder_to_node_id,
      node_id_to_preorder,
    }
  }

  fn build_selector_preorder_mapping(&self) -> SelectorDomMapping {
    self
      .build_selector_preorder_mapping_from(self.root)
      .unwrap_or_else(|| {
        debug_assert!(false, "dom2 document root missing");
        SelectorDomMapping {
          preorder_to_node_id: vec![None],
          node_id_to_preorder: vec![0; self.nodes.len()],
        }
      })
  }

  fn build_selector_preorder_mapping_from(&self, root: NodeId) -> Option<SelectorDomMapping> {
    if root.index() >= self.nodes.len() {
      return None;
    }

    // DocumentFragment.querySelector(All) uses a synthetic scoping root element in the renderer
    // snapshot (see `to_renderer_dom_subtree`). Adjust the mapping so preorder ids stay aligned:
    // - preorder id 1 corresponds to the synthetic root (maps to None)
    // - the real DocumentFragment node is omitted (cannot be returned from selectors)
    let fragment_root = matches!(self.node(root).kind, NodeKind::DocumentFragment);

    // Preorder ids are 1-based (index 0 unused), matching `crate::dom::enumerate_dom_ids`.
    let mut preorder_to_node_id: Vec<Option<NodeId>> =
      Vec::with_capacity(self.nodes.len() + 1 + usize::from(fragment_root));
    preorder_to_node_id.push(None);
    let mut node_id_to_preorder: Vec<usize> = vec![0; self.nodes.len()];

    enum StackItem {
      Real(NodeId),
      SyntheticWbrZwsp(NodeId),
    }

    fn node_is_renderable(kind: &NodeKind) -> bool {
      // Keep selector preorder ids aligned with `to_renderer_dom` / `to_renderer_dom_subtree`, which
      // drop html5ever-only nodes such as comments and doctypes.
      !matches!(
        kind,
        NodeKind::Comment { .. }
          | NodeKind::ProcessingInstruction { .. }
          | NodeKind::Doctype { .. }
      )
    }

    let mut stack: Vec<StackItem> = Vec::new();
    if fragment_root {
      preorder_to_node_id.push(None);
      let node = self.node(root);
      for &child in node.children.iter().rev() {
        let Some(child_node) = self.nodes.get(child.index()) else {
          continue;
        };
        if child_node.parent == Some(root) {
          stack.push(StackItem::Real(child));
        }
      }
    } else {
      stack.push(StackItem::Real(root));
    }
    while let Some(item) = stack.pop() {
      match item {
        StackItem::Real(id) => {
          if id.index() >= self.nodes.len() {
            continue;
          }

          let preorder_id = preorder_to_node_id.len();
          let node = self.node(id);
          if !node_is_renderable(&node.kind) {
            continue;
          }

          let mapped_node_id = match &node.kind {
            // Selector queries on DocumentFragment should never return the fragment itself, but we
            // still need a root entry so preorder ids stay aligned with the renderer snapshot used
            // for selector matching.
            NodeKind::DocumentFragment => None,
            _ => Some(id),
          };

          preorder_to_node_id.push(mapped_node_id);
          if mapped_node_id.is_some() {
            node_id_to_preorder[id.index()] = preorder_id;
          }

          if self.should_inject_wbr_zwsp(id) {
            stack.push(StackItem::SyntheticWbrZwsp(id));
          }

          // For selector matching we treat inert subtrees (currently: `<template>` contents) as
          // disconnected, mirroring how template `.content` is not in the light DOM tree.
          if !node.inert_subtree {
            for &child in node.children.iter().rev() {
              let Some(child_node) = self.nodes.get(child.index()) else {
                continue;
              };
              if child_node.parent == Some(id) {
                stack.push(StackItem::Real(child));
              }
            }
          }
        }
        StackItem::SyntheticWbrZwsp(parent) => {
          // Synthetic ZWSP nodes map back to their parent `<wbr>` element `NodeId`.
          preorder_to_node_id.push(Some(parent));
        }
      }
    }

    Some(SelectorDomMapping {
      preorder_to_node_id,
      node_id_to_preorder,
    })
  }

  fn wrap_selector_subtree_snapshot_in_document(
    &self,
    dom: DomNode,
    mut mapping: SelectorDomMapping,
  ) -> (DomNode, SelectorDomMapping) {
    // Selector matching needs document-level flags (notably `is_html_document`) even when the
    // selector entry point is a disconnected subtree (DocumentFragment, detached element, etc).
    // Wrap subtree snapshots in a synthetic Document node so `ElementRef` can always find the
    // document ancestor during matching.
    mapping.preorder_to_node_id.insert(1, None);
    for v in mapping.node_id_to_preorder.iter_mut() {
      if *v != 0 {
        *v += 1;
      }
    }

    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: self.scripting_enabled,
        is_html_document: self.is_html_document(),
      },
      children: vec![dom],
    };

    (dom, mapping)
  }

  fn selector_snapshot(&mut self) -> (Arc<DomNode>, Arc<SelectorDomMapping>) {
    let generation = self.mutation_generation;
    let nodes_len = self.nodes.len();
    let scripting_enabled = self.scripting_enabled;
    let rebuild = match self.selector_snapshot_cache.as_ref() {
      Some(cache) => {
        cache.generation != generation
          || cache.nodes_len != nodes_len
          || cache.scripting_enabled != scripting_enabled
      }
      None => true,
    };

    if rebuild {
      let dom = Arc::new(self.to_renderer_dom());
      let mapping = Arc::new(self.build_selector_preorder_mapping());
      self.selector_snapshot_cache = Some(SelectorSnapshotCache {
        generation,
        nodes_len,
        scripting_enabled,
        dom,
        mapping,
      });
    }

    if let Some(cache) = self.selector_snapshot_cache.as_ref() {
      (Arc::clone(&cache.dom), Arc::clone(&cache.mapping))
    } else {
      let dom = Arc::new(self.to_renderer_dom());
      let mapping = Arc::new(self.build_selector_preorder_mapping());
      self.selector_snapshot_cache = Some(SelectorSnapshotCache {
        generation,
        nodes_len,
        scripting_enabled,
        dom: Arc::clone(&dom),
        mapping: Arc::clone(&mapping),
      });
      (dom, mapping)
    }
  }

  fn is_html_document_for_dom_selectors(&self) -> bool {
    // DOM's notion of an "HTML document" is ultimately driven by how the document was created /
    // parsed (HTML parser vs XML parser). `dom2` currently does not have a full XML parser, so we
    // approximate by treating documents whose document element is an HTML `<html>` element as HTML
    // documents for selector parsing.
    let Some(document_element) = self.document_element() else {
      return true;
    };
    match &self.node(document_element).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => (namespace.is_empty() || namespace == HTML_NAMESPACE) && tag_name.eq_ignore_ascii_case("html"),
      NodeKind::Slot { namespace, .. } => namespace.is_empty() || namespace == HTML_NAMESPACE,
      _ => false,
    }
  }

  fn compute_in_scope_namespace_prefix_map_for_dom_selectors(
    &self,
    element: NodeId,
  ) -> (Option<String>, Vec<(String, String)>) {
    // In-scope namespace declarations are defined by the `xmlns` / `xmlns:*` attributes on the
    // element and its ancestors. Declarations are case-sensitive and the nearest ancestor wins.
    let mut default_ns_seen = false;
    let mut default_ns: Option<String> = None;
    let mut prefixes_seen: HashSet<String> = HashSet::new();
    let mut prefixes: HashMap<String, String> = HashMap::new();

    // Reserved `xml` prefix always maps to the XML namespace.
    prefixes.insert("xml".to_string(), XML_NAMESPACE.to_string());
    prefixes_seen.insert("xml".to_string());

    let mut current = Some(element);
    // Defensive bound to avoid infinite loops on corrupted trees.
    let mut remaining = self.nodes.len() + 1;
    while let Some(node_id) = current {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      let node = self.node(node_id);
      let attrs: &[Attribute] = match &node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => {
          current = node.parent;
          continue;
        }
      };

      for attr in attrs {
        if attr.namespace != XMLNS_NAMESPACE {
          continue;
        }
        if attr.prefix.is_none() && attr.local_name == "xmlns" {
          if !default_ns_seen {
            default_ns_seen = true;
            // `xmlns=""` unbinds the default namespace.
            if attr.value.is_empty() {
              default_ns = None;
            } else {
              default_ns = Some(attr.value.clone());
            }
          }
          continue;
        }

        let Some(_xmlns) = attr.prefix.as_deref().filter(|p| *p == "xmlns") else {
          continue;
        };
        let prefix = attr.local_name.as_str();
        // Ignore malformed namespace declarations (empty prefix or multiple colons).
        if prefix.is_empty() || prefix.contains(':') {
          continue;
        }
        // `xml` prefix is reserved and cannot be redeclared.
        if prefix == "xml" {
          continue;
        }
        // `xmlns` is reserved for namespace declaration attributes.
        if prefix == "xmlns" {
          continue;
        }

        if prefixes_seen.contains(prefix) {
          continue;
        }
        prefixes_seen.insert(prefix.to_string());

        // `xmlns:prefix=""` unbinds the prefix; treat it as shadowing any ancestor binding.
        if attr.value.is_empty() {
          continue;
        }
        prefixes.insert(prefix.to_string(), attr.value.clone());
      }

      current = node.parent;
    }

    (default_ns, prefixes.into_iter().collect())
  }

  fn namespace_context_for_dom_selector_parsing(
    &self,
    scope: Option<NodeId>,
  ) -> (Option<String>, Vec<(String, String)>) {
    if self.is_html_document_for_dom_selectors() {
      return (None, Vec::new());
    }

    // Per DOM, namespace prefix maps are derived from the selector scope node:
    // - Document: documentElement.
    // - Element: the element itself.
    // - DocumentFragment / ShadowRoot: empty (acceptable MVP).
    let scope_element = match scope {
      None => self.document_element(),
      Some(node_id) => match &self.node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => Some(node_id),
        _ => None,
      },
    };

    match scope_element {
      Some(element) => self.compute_in_scope_namespace_prefix_map_for_dom_selectors(element),
      None => (None, vec![("xml".to_string(), XML_NAMESPACE.to_string())]),
    }
  }

  pub fn to_renderer_dom_with_mapping(&self) -> RendererDomSnapshot {
    RendererDomSnapshot {
      dom: self.to_renderer_dom(),
      mapping: self.build_renderer_preorder_mapping(),
    }
  }

  /// Build a renderer preorder ↔ `dom2` [`NodeId`] mapping without cloning a renderer [`DomNode`]
  /// snapshot tree.
  ///
  /// This mirrors the preorder traversal semantics used by [`Document::to_renderer_dom_with_mapping`]
  /// (1-based preorder ids as produced by [`crate::dom::enumerate_dom_ids`], including inert template
  /// contents + declarative shadow roots, and mapping synthetic nodes such as `<wbr>`'s implicit ZWSP
  /// child back to the owning real `NodeId`).
  pub fn renderer_dom_mapping(&self) -> RendererDomMapping {
    self.build_renderer_preorder_mapping()
  }

  pub fn query_selector(
    &mut self,
    selectors: &str,
    scope: Option<NodeId>,
  ) -> Result<Option<NodeId>, DomException> {
    let dom_is_html = self.is_html_document_for_dom_selectors();
    let (default_ns, prefixes) = self.namespace_context_for_dom_selector_parsing(scope);
    let selector_list = parse_selector_list_for_dom(
      dom_is_html,
      default_ns.as_deref(),
      &prefixes,
      selectors,
    )?;
    let quirks_mode = match &self.node(self.root()).kind {
      NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => QuirksMode::NoQuirks,
    };

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let use_document_snapshot =
      scope.is_none() || scope.is_some_and(|id| self.is_connected_for_scripting(id));
    let scope_is_virtual = scope.is_some_and(|scope_id| {
      matches!(
        self.node(scope_id).kind,
        NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. }
      )
    });
    let scope_host = if scope_is_virtual && use_document_snapshot {
      scope.and_then(|scope_id| match &self.node(scope_id).kind {
        NodeKind::ShadowRoot { .. } => self.parent_node(scope_id),
        _ => None,
      })
    } else {
      None
    };

    let (snapshot_dom, mapping) = if use_document_snapshot {
      self.selector_snapshot()
    } else {
      let Some(scope_id) = scope else {
        return Ok(None);
      };
      let Some(mut dom) = self.to_renderer_dom_subtree(scope_id) else {
        return Ok(None);
      };
      let Some(mut mapping) = self.build_selector_preorder_mapping_from(scope_id) else {
        return Ok(None);
      };

      if scope_is_virtual {
        // Selectors4 defines `:scope` for DocumentFragments and ShadowRoots as a virtual scoping root:
        // it is featureless, cannot be the subject of the selector, and acts as the parent of any
        // top-level elements. The upstream `selectors` crate models this featureless-parent behavior
        // via shadow-root traversal (see `next_element_for_combinator`).
        //
        // To support `DocumentFragment.querySelector(":scope > span")`, wrap the fragment subtree in a
        // synthetic shadow root under a synthetic host element. The synthetic host is mapped to the
        // fragment's `NodeId` so scope activation still works, but we filter it out from results since
        // document fragments cannot be querySelector subjects.
        let children = std::mem::take(&mut dom.children);
        let shadow_root = DomNode {
          node_type: DomNodeType::ShadowRoot {
            mode: ShadowRootMode::Open,
            delegates_focus: false,
          },
          children,
        };
        dom = DomNode {
          node_type: DomNodeType::Element {
            tag_name: "__fastrender_scope".to_string(),
            namespace: String::new(),
            attributes: Vec::new(),
          },
          children: vec![shadow_root],
        };

        let mut preorder_to_node_id: Vec<Option<NodeId>> =
          Vec::with_capacity(mapping.preorder_to_node_id.len() + 1);
        preorder_to_node_id.push(None);
        preorder_to_node_id.push(Some(scope_id));
        preorder_to_node_id.push(Some(scope_id));
        preorder_to_node_id.extend_from_slice(&mapping.preorder_to_node_id[2..]);
        mapping.preorder_to_node_id = preorder_to_node_id;
        for v in mapping.node_id_to_preorder.iter_mut() {
          if *v >= 2 {
            *v += 1;
          }
        }
      }

      let (dom, mapping) = self.wrap_selector_subtree_snapshot_in_document(dom, mapping);
      (Arc::new(dom), Arc::new(mapping))
    };

    let snapshot_dom = snapshot_dom.as_ref();
    let mapping = mapping.as_ref();

    // If we're searching the full document snapshot, ensure the scope is reachable inside the
    // selector snapshot mapping. Detached/inert scopes use subtree snapshots instead.
    if use_document_snapshot {
      let scope_preorder = scope.and_then(|id| mapping.preorder_for_node_id(id));
      if scope.is_some() && scope_preorder.is_none() {
        return Ok(None);
      }
    }

    // `document.querySelector` (and `Element.querySelector`) do not pierce shadow roots by default.
    // If a scope is provided inside a shadow tree, allow matching only inside that same shadow root.
    let allowed_shadow_root = if scope_is_virtual {
      // Virtual scoping roots are treated as their own shadow-root boundary for selector matching
      // (`:scope >` selectors should match direct children).
      scope
    } else {
      scope.and_then(|scope_id| {
        self
          .ancestors(scope_id)
          .find(|&ancestor| matches!(self.node(ancestor).kind, NodeKind::ShadowRoot { .. }))
      })
    };

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: Option<NodeId>,
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut shadow_root_stack: Vec<NodeId> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: snapshot_dom,
      exiting: false,
      node_id: None,
    });
    let mut next_preorder_id = 1usize;
    let mut scope_active = scope.is_none() || !use_document_snapshot;
    let mut scope_anchor: Option<OpaqueElement> = (!use_document_snapshot
      && snapshot_dom.is_element())
    .then_some(OpaqueElement::new(snapshot_dom));
    let scope_is_element = scope.is_some_and(|id| {
      matches!(
        self.node(id).kind,
        NodeKind::Element { .. } | NodeKind::Slot { .. }
      )
    });

    while let Some(item) = stack.pop() {
      if item.exiting {
        if matches!(&item.node.node_type, DomNodeType::ShadowRoot { .. }) {
          if let Some(id) = item.node_id {
            if shadow_root_stack.last() == Some(&id) {
              shadow_root_stack.pop();
            }
          }
        }
        ancestors.pop();
        if let Some(scope_id) = scope {
          if item.node_id == Some(scope_id) {
            break;
          }
        }
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping.node_id_for_preorder(preorder_id);

      if matches!(&item.node.node_type, DomNodeType::ShadowRoot { .. }) {
        if let Some(id) = dom2_id {
          shadow_root_stack.push(id);
        }
      }

      if let Some(dom2_id) = dom2_id {
        if Some(dom2_id) == scope_host && scope_anchor.is_none() && item.node.is_element() {
          scope_anchor = Some(OpaqueElement::new(item.node));
        }
        if scope == Some(dom2_id) {
          scope_active = true;
          if item.node.is_element() {
            scope_anchor = Some(OpaqueElement::new(item.node));
          }
        }

        let current_shadow_root = shadow_root_stack.last().copied();
        let shadow_ok = allowed_shadow_root.map_or(current_shadow_root.is_none(), |allowed| {
          current_shadow_root == Some(allowed)
        });

        if scope_active && item.node.is_element() && shadow_ok {
          let kind_ok = matches!(
            self.node(dom2_id).kind,
            NodeKind::Element { .. } | NodeKind::Slot { .. }
          );
          if kind_ok
            && node_matches_selector_list(
              item.node,
              &ancestors,
              &selector_list,
              &mut selector_caches,
              quirks_mode,
              scope_anchor,
            )
          {
            if scope == Some(dom2_id) && !scope_is_element {
            } else {
              return Ok(Some(dom2_id));
            }
          }
        }
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      let mut descend = true;
      if let Some(dom2_id) = dom2_id {
        descend = !self.node(dom2_id).inert_subtree;
      }

      if descend {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: None,
          });
        }
      }
    }

    Ok(None)
  }

  pub fn query_selector_all(
    &mut self,
    selectors: &str,
    scope: Option<NodeId>,
  ) -> Result<Vec<NodeId>, DomException> {
    let dom_is_html = self.is_html_document_for_dom_selectors();
    let (default_ns, prefixes) = self.namespace_context_for_dom_selector_parsing(scope);
    let selector_list = parse_selector_list_for_dom(
      dom_is_html,
      default_ns.as_deref(),
      &prefixes,
      selectors,
    )?;
    let quirks_mode = match &self.node(self.root()).kind {
      NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => QuirksMode::NoQuirks,
    };

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let scope_is_virtual = scope.is_some_and(|scope_id| {
      matches!(
        self.node(scope_id).kind,
        NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. }
      )
    });
    let use_document_snapshot =
      scope.is_none() || scope.is_some_and(|id| self.is_connected_for_scripting(id));
    let scope_host = if scope_is_virtual && use_document_snapshot {
      scope.and_then(|scope_id| match &self.node(scope_id).kind {
        NodeKind::ShadowRoot { .. } => self.parent_node(scope_id),
        _ => None,
      })
    } else {
      None
    };

    let (snapshot_dom, mapping) = if use_document_snapshot {
      self.selector_snapshot()
    } else {
      let Some(scope_id) = scope else {
        return Ok(Vec::new());
      };
      let Some(mut dom) = self.to_renderer_dom_subtree(scope_id) else {
        return Ok(Vec::new());
      };
      let Some(mut mapping) = self.build_selector_preorder_mapping_from(scope_id) else {
        return Ok(Vec::new());
      };

      if scope_is_virtual {
        let children = std::mem::take(&mut dom.children);
        let shadow_root = DomNode {
          node_type: DomNodeType::ShadowRoot {
            mode: ShadowRootMode::Open,
            delegates_focus: false,
          },
          children,
        };
        dom = DomNode {
          node_type: DomNodeType::Element {
            tag_name: "__fastrender_scope".to_string(),
            namespace: String::new(),
            attributes: Vec::new(),
          },
          children: vec![shadow_root],
        };

        let mut preorder_to_node_id: Vec<Option<NodeId>> =
          Vec::with_capacity(mapping.preorder_to_node_id.len() + 1);
        preorder_to_node_id.push(None);
        preorder_to_node_id.push(Some(scope_id));
        preorder_to_node_id.push(Some(scope_id));
        preorder_to_node_id.extend_from_slice(&mapping.preorder_to_node_id[2..]);
        mapping.preorder_to_node_id = preorder_to_node_id;
        for v in mapping.node_id_to_preorder.iter_mut() {
          if *v >= 2 {
            *v += 1;
          }
        }
      }

      let (dom, mapping) = self.wrap_selector_subtree_snapshot_in_document(dom, mapping);
      (Arc::new(dom), Arc::new(mapping))
    };

    let snapshot_dom = snapshot_dom.as_ref();
    let mapping = mapping.as_ref();

    if use_document_snapshot {
      let scope_preorder = scope.and_then(|id| mapping.preorder_for_node_id(id));
      if scope.is_some() && scope_preorder.is_none() {
        return Ok(Vec::new());
      }
    }

    let allowed_shadow_root = if scope_is_virtual {
      scope
    } else {
      scope.and_then(|scope_id| {
        self
          .ancestors(scope_id)
          .find(|&ancestor| matches!(self.node(ancestor).kind, NodeKind::ShadowRoot { .. }))
      })
    };

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: Option<NodeId>,
    }

    let mut results: Vec<NodeId> = Vec::new();
    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut shadow_root_stack: Vec<NodeId> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: snapshot_dom,
      exiting: false,
      node_id: None,
    });
    let mut next_preorder_id = 1usize;
    let mut scope_active = scope.is_none() || !use_document_snapshot;
    let mut scope_anchor: Option<OpaqueElement> = (!use_document_snapshot
      && snapshot_dom.is_element())
    .then_some(OpaqueElement::new(snapshot_dom));
    let scope_is_element = scope.is_some_and(|id| {
      matches!(
        self.node(id).kind,
        NodeKind::Element { .. } | NodeKind::Slot { .. }
      )
    });

    while let Some(item) = stack.pop() {
      if item.exiting {
        if matches!(&item.node.node_type, DomNodeType::ShadowRoot { .. }) {
          if let Some(id) = item.node_id {
            if shadow_root_stack.last() == Some(&id) {
              shadow_root_stack.pop();
            }
          }
        }
        ancestors.pop();
        if let Some(scope_id) = scope {
          if item.node_id == Some(scope_id) {
            break;
          }
        }
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping.node_id_for_preorder(preorder_id);

      if matches!(&item.node.node_type, DomNodeType::ShadowRoot { .. }) {
        if let Some(id) = dom2_id {
          shadow_root_stack.push(id);
        }
      }

      if let Some(dom2_id) = dom2_id {
        if Some(dom2_id) == scope_host && scope_anchor.is_none() && item.node.is_element() {
          scope_anchor = Some(OpaqueElement::new(item.node));
        }
        if scope == Some(dom2_id) {
          scope_active = true;
          if item.node.is_element() {
            scope_anchor = Some(OpaqueElement::new(item.node));
          }
        }

        let current_shadow_root = shadow_root_stack.last().copied();
        let shadow_ok = allowed_shadow_root.map_or(current_shadow_root.is_none(), |allowed| {
          current_shadow_root == Some(allowed)
        });

        if scope_active && item.node.is_element() && shadow_ok {
          let kind_ok = matches!(
            self.node(dom2_id).kind,
            NodeKind::Element { .. } | NodeKind::Slot { .. }
          );
          if kind_ok
            && node_matches_selector_list(
              item.node,
              &ancestors,
              &selector_list,
              &mut selector_caches,
              quirks_mode,
              scope_anchor,
            )
          {
            if scope == Some(dom2_id) && !scope_is_element {
            } else {
              results.push(dom2_id);
            }
          }
        }
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      let mut descend = true;
      if let Some(dom2_id) = dom2_id {
        descend = !self.node(dom2_id).inert_subtree;
      }

      if descend {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: None,
          });
        }
      }
    }

    Ok(results)
  }

  pub fn matches_selector(
    &mut self,
    element: NodeId,
    selectors: &str,
  ) -> Result<bool, DomException> {
    let dom_is_html = self.is_html_document_for_dom_selectors();
    let (default_ns, prefixes) = self.namespace_context_for_dom_selector_parsing(Some(element));
    let selector_list = parse_selector_list_for_dom(
      dom_is_html,
      default_ns.as_deref(),
      &prefixes,
      selectors,
    )?;
    Ok(self.matches_selector_list(element, &selector_list))
  }

  fn matches_selector_list(
    &mut self,
    element: NodeId,
    selector_list: &SelectorList<FastRenderSelectorImpl>,
  ) -> bool {
    if element.index() >= self.nodes.len() {
      return false;
    }
    match &self.node(element).kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
      _ => return false,
    }

    let quirks_mode = match &self.node(self.root()).kind {
      NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => QuirksMode::NoQuirks,
    };

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let (snapshot_dom, mapping) = if self.is_connected_for_scripting(element) {
      self.selector_snapshot()
    } else {
      // If the element is disconnected (either detached or inside inert `<template>` contents), we
      // still want to be able to match selectors against it and its connected ancestors within that
      // disconnected region. Stop at the first detached ancestor or at the boundary of an inert
      // subtree so selectors do not cross template `.content` boundaries.
      let mut root = element;
      while let Some(parent) = self.parent_node(root) {
        if self.node(parent).inert_subtree {
          break;
        }
        root = parent;
      }

      let Some(dom) = self.to_renderer_dom_subtree(root) else {
        return false;
      };
      let Some(mapping) = self.build_selector_preorder_mapping_from(root) else {
        return false;
      };
      let (dom, mapping) = self.wrap_selector_subtree_snapshot_in_document(dom, mapping);
      (Arc::new(dom), Arc::new(mapping))
    };
    let snapshot_dom = snapshot_dom.as_ref();
    let mapping = mapping.as_ref();

    let Some(target_preorder) = mapping.preorder_for_node_id(element) else {
      return false;
    };

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: snapshot_dom,
      exiting: false,
    });
    let mut next_preorder_id = 1usize;

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping.node_id_for_preorder(preorder_id);

      stack.push(StackItem {
        node: item.node,
        exiting: true,
      });
      ancestors.push(item.node);

      if let Some(dom2_id) = dom2_id {
        if dom2_id == element {
          let anchor = Some(OpaqueElement::new(item.node));
          let matched = node_matches_selector_list(
            item.node,
            &ancestors[..ancestors.len().saturating_sub(1)],
            selector_list,
            &mut selector_caches,
            quirks_mode,
            anchor,
          );
          return matched;
        }

        if preorder_id >= target_preorder {
          // If we've passed the target preorder id without finding it, the mapping/traversal is out
          // of sync; bail out defensively.
          return false;
        }
      }

      let mut descend = true;
      if let Some(dom2_id) = dom2_id {
        descend = !self.node(dom2_id).inert_subtree;
      }

      if descend {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
          });
        }
      }
    }

    false
  }

  /// `Element.closest(selectors)` for a `dom2` element.
  ///
  /// This walks up the ancestor chain (including `element` itself) and returns the first element
  /// that matches `selectors`.
  ///
  /// Inert `<template>` contents are treated as disconnected: traversal stops before the inert
  /// template boundary so `closest()` does not see ancestors outside the template's `.content`
  /// subtree.
  ///
  /// Shadow roots are tree-scope boundaries: traversal stops before a `ShadowRoot` node so
  /// `closest()` does not cross from a shadow tree to its host element.
  pub fn closest(
    &mut self,
    element: NodeId,
    selectors: &str,
  ) -> Result<Option<NodeId>, DomException> {
    if element.index() >= self.nodes.len() {
      return Ok(None);
    }
    match &self.node(element).kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
      _ => return Ok(None),
    }

    let dom_is_html = self.is_html_document_for_dom_selectors();
    let (default_ns, prefixes) = self.namespace_context_for_dom_selector_parsing(Some(element));
    let selector_list = parse_selector_list_for_dom(
      dom_is_html,
      default_ns.as_deref(),
      &prefixes,
      selectors,
    )?;

    let mut current = element;
    loop {
      // The DOM Standard defines `Element.closest()` in terms of the element's inclusive ancestors
      // in the *tree*; it must not cross a shadow root boundary. In `dom2`, shadow roots are stored
      // as nodes whose `parent` points at the host, so we need an explicit boundary check here to
      // avoid returning the host when called from inside a shadow tree.
      if matches!(self.node(current).kind, NodeKind::ShadowRoot { .. }) {
        return Ok(None);
      }

      if self.matches_selector_list(current, &selector_list) {
        return Ok(Some(current));
      }

      let mut cursor = current;
      loop {
        let Some(parent) = self.parent_node(cursor) else {
          return Ok(None);
        };
        let parent_node = self.node(parent);
        if matches!(&parent_node.kind, NodeKind::ShadowRoot { .. }) || parent_node.inert_subtree {
          return Ok(None);
        }
        if matches!(&parent_node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }) {
          current = parent;
          break;
        }
        cursor = parent;
      }
    }
  }
}

pub fn get_element_by_id(doc: &Document, id: &str) -> Option<NodeId> {
  doc.get_element_by_id(id)
}

pub fn set_attribute(doc: &mut Document, node: NodeId, name: &str, value: &str) -> bool {
  doc.set_attribute(node, name, value).unwrap_or(false)
}

#[cfg(test)]
mod attrs_tests;
#[cfg(test)]
mod class_list_tests;
#[cfg(test)]
mod contextual_fragment_tests;
#[cfg(test)]
mod cross_document_tests;
#[cfg(test)]
mod qualified_name_tests;
#[cfg(test)]
mod html5ever_sink_tests;
#[cfg(test)]
#[cfg(test)]
mod html_tests;
#[cfg(test)]
mod inner_html_tests;
#[cfg(test)]
mod live_mutation_tests;
#[cfg(test)]
mod node_iterator_tests;
#[cfg(test)]
mod mapping_tests;
#[cfg(test)]
mod mutation_generation_tests;
#[cfg(test)]
mod mutation_log_tests;
#[cfg(test)]
mod mutation_tests;
#[cfg(test)]
mod mutation_observer_tests;
#[cfg(test)]
mod mutation_observer_remap_tests;
#[cfg(test)]
mod mutation_observer_transient_tests;
#[cfg(test)]
mod mutation_observer_shared_agent_tests;
#[cfg(test)]
mod forms_tests;
#[cfg(test)]
mod query_tests;
#[cfg(test)]
mod get_elements_by_tests;
#[cfg(test)]
mod script_internal_slots_tests;
#[cfg(test)]
mod selector_query_tests;
#[cfg(test)]
mod selectors_detached_tests;
#[cfg(test)]
mod xml_namespace_selector_tests;
#[cfg(test)]
mod shadow_boundary_tests;
#[cfg(test)]
mod shadow_root_boundary_tests;
#[cfg(test)]
mod xml_parse_tests;
#[cfg(test)]
mod live_range_updates_tests;
mod range_tests;
#[cfg(test)]
mod xml_selector_disconnected_tests;
#[cfg(test)]
mod xml_selector_tests;
#[cfg(test)]
mod wbr_tests;
#[cfg(test)]
mod xml_document_tests;

#[cfg(test)]
mod helper_tests {
  use super::*;
  use crate::dom::parse_html;

  fn find_tag(doc: &Document, id: NodeId) -> Option<&str> {
    match &doc.node(id).kind {
      NodeKind::Element { tag_name, .. } => Some(tag_name.as_str()),
      _ => None,
    }
  }

  #[test]
  fn head_and_body_return_elements_under_html_root() {
    let root = parse_html("<!doctype html><html><head></head><body></body></html>").unwrap();
    let doc = Document::from_renderer_dom(&root);

    let head = doc.head().expect("expected head");
    let body = doc.body().expect("expected body");
    assert!(find_tag(&doc, head).is_some_and(|t| t.eq_ignore_ascii_case("head")));
    assert!(find_tag(&doc, body).is_some_and(|t| t.eq_ignore_ascii_case("body")));
  }

  #[test]
  fn head_and_body_are_none_without_html_document_element() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        prefix: None,
        attributes: Vec::new(),
      },
      Some(doc.root()),
      /* inert_subtree */ false,
    );

    assert_eq!(doc.head(), None);
    assert_eq!(doc.body(), None);
  }

  #[test]
  fn head_and_body_return_first_matching_child() {
    let root = parse_html(
      "<!doctype html><html><head id=a></head><head id=b></head><body id=c></body><body id=d></body></html>",
    )
    .unwrap();
    let doc = Document::from_renderer_dom(&root);

    let head = doc.head().expect("expected head");
    let body = doc.body().expect("expected body");
    assert_eq!(doc.get_attribute(head, "id").unwrap(), Some("a"));
    assert_eq!(doc.get_attribute(body, "id").unwrap(), Some("c"));
  }

  #[test]
  fn body_returns_frameset_when_no_body() {
    let root =
      parse_html("<!doctype html><html><head></head><frameset id=a></frameset></html>").unwrap();
    let doc = Document::from_renderer_dom(&root);

    let body = doc.body().expect("expected frameset");
    assert!(find_tag(&doc, body).is_some_and(|t| t.eq_ignore_ascii_case("frameset")));
    assert_eq!(doc.get_attribute(body, "id").unwrap(), Some("a"));
  }

  #[test]
  fn body_prefers_body_over_frameset() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let html = doc.push_node(
      NodeKind::Element {
        tag_name: "html".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        prefix: None,
        attributes: Vec::new(),
      },
      Some(doc.root()),
      /* inert_subtree */ false,
    );
    doc.push_node(
      NodeKind::Element {
        tag_name: "frameset".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        prefix: None,
        attributes: vec![("id".to_string(), "fs".to_string())],
      },
      Some(html),
      /* inert_subtree */ false,
    );
    let body = doc.push_node(
      NodeKind::Element {
        tag_name: "body".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        prefix: None,
        attributes: vec![("id".to_string(), "b".to_string())],
      },
      Some(html),
      /* inert_subtree */ false,
    );

    assert_eq!(doc.body(), Some(body));
  }
}

#[cfg(test)]
mod selector_snapshot_cache_tests {
  use super::Document;
  use selectors::context::QuirksMode;
  use std::sync::Arc;

  #[test]
  fn selector_snapshot_cache_reuses_snapshot_when_document_is_unchanged() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let div = doc.create_element("div", "");
    doc.append_child(root, div).unwrap();

    let (dom1, mapping1) = doc.selector_snapshot();
    let (dom2, mapping2) = doc.selector_snapshot();

    assert!(Arc::ptr_eq(&dom1, &dom2));
    assert!(Arc::ptr_eq(&mapping1, &mapping2));
  }

  #[test]
  fn selector_snapshot_cache_rebuilds_when_mutation_generation_changes() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let div = doc.create_element("div", "");
    doc.append_child(root, div).unwrap();

    let (dom1, mapping1) = doc.selector_snapshot();

    let span = doc.create_element("span", "");
    doc.append_child(div, span).unwrap();

    let (dom2, mapping2) = doc.selector_snapshot();
    assert!(!Arc::ptr_eq(&dom1, &dom2));
    assert!(!Arc::ptr_eq(&mapping1, &mapping2));
  }

  #[test]
  fn selector_snapshot_cache_rebuilds_when_nodes_len_changes() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let div = doc.create_element("div", "");
    doc.append_child(root, div).unwrap();

    let (dom1, mapping1) = doc.selector_snapshot();

    let _detached = doc.create_element("span", "");

    let (dom2, mapping2) = doc.selector_snapshot();
    assert!(!Arc::ptr_eq(&dom1, &dom2));
    assert!(!Arc::ptr_eq(&mapping1, &mapping2));
  }
}

#[cfg(test)]
mod template_inert_tests {
  use super::{Document, NodeId, NodeKind};

  fn find_node_by_id_attribute(doc: &Document, id: &str) -> Option<NodeId> {
    if id.is_empty() {
      return None;
    }
    doc.nodes().iter().enumerate().find_map(|(idx, node)| {
      let attrs = match &node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return None,
      };
      attrs
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
        .then_some(NodeId(idx))
    })
  }

  #[test]
  fn query_selector_and_get_element_by_id_ignore_inert_template_descendants() {
    let root = crate::dom::parse_html(
      "<!doctype html><html><body>\
       <template><div id=inside></div></template>\
       <div id=outside></div>\
       </body></html>",
    )
    .unwrap();
    let mut doc = Document::from_renderer_dom(&root);

    assert_eq!(doc.query_selector("#inside", None).unwrap(), None);
    assert_eq!(doc.get_element_by_id("inside"), None);

    let outside_qs = doc.query_selector("#outside", None).unwrap();
    let outside_id = doc.get_element_by_id("outside");
    assert!(
      outside_qs.is_some(),
      "expected #outside to be query-selectable"
    );
    assert_eq!(outside_qs, outside_id);
  }

  #[test]
  fn matches_selector_works_for_inert_template_descendants_but_does_not_cross_boundary() {
    let root = crate::dom::parse_html(
      "<!doctype html><html><body>\
       <template><div id=inside></div></template>\
       </body></html>",
    )
    .unwrap();
    let mut doc = Document::from_renderer_dom(&root);

    let inside = find_node_by_id_attribute(&doc, "inside").expect("expected inside node in tree");
    assert!(
      doc.is_descendant_of_inert_template(inside),
      "inside node should be inside inert template subtree"
    );
    assert!(
      doc.matches_selector(inside, "#inside").unwrap(),
      "matches_selector should still work when querying inert template descendants directly"
    );
    assert!(
      !doc.matches_selector(inside, "body #inside").unwrap(),
      "matches_selector must not cross inert <template> boundaries into the document tree"
    );
  }

  #[test]
  fn closest_stops_at_inert_template_boundary() {
    let root = crate::dom::parse_html(
      "<!doctype html><html><body>\
       <template><div id=inside></div></template>\
       </body></html>",
    )
    .unwrap();
    let mut doc = Document::from_renderer_dom(&root);

    let inside = find_node_by_id_attribute(&doc, "inside").expect("expected inside node in tree");
    assert!(
      doc.is_descendant_of_inert_template(inside),
      "inside node should be inside inert template subtree"
    );

    // `closest()` is inclusive and should match the node itself.
    assert_eq!(doc.closest(inside, "#inside").unwrap(), Some(inside));

    // Inert `<template>` contents are disconnected from the light DOM; traversal must not reach
    // ancestors outside the template's `.content` subtree.
    assert_eq!(doc.closest(inside, "body").unwrap(), None);
  }

  #[test]
  fn declarative_shadow_root_promotion_does_not_make_shadow_root_inert() {
    // `parse_html` promotes the first `<template shadowroot=...>` child into a ShadowRoot node,
    // leaving subsequent shadowroot templates as ordinary inert `<template>` elements.
    let root = crate::dom::parse_html(
      "<!doctype html><html><body>\
       <div id=host>\
         <template shadowroot=open><span id=shadow></span></template>\
         <template shadowroot=open><span id=inert></span></template>\
       </div>\
       </body></html>",
    )
    .unwrap();
    let doc = Document::from_renderer_dom(&root);

    let shadow_root_id = doc.nodes().iter().enumerate().find_map(|(idx, node)| {
      matches!(node.kind, NodeKind::ShadowRoot { .. }).then_some(NodeId(idx))
    });
    let shadow_root_id = shadow_root_id.expect("expected promoted ShadowRoot node");
    assert!(
      !doc.node(shadow_root_id).inert_subtree,
      "promoted ShadowRoot nodes must not be inert"
    );

    let inert_template_id = doc.nodes().iter().enumerate().find_map(|(idx, node)| {
      let NodeKind::Element {
        tag_name,
        attributes,
        ..
      } = &node.kind
      else {
        return None;
      };
      if !tag_name.eq_ignore_ascii_case("template") {
        return None;
      }
      attributes
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("shadowroot"))
        .then_some(NodeId(idx))
    });
    let inert_template_id = inert_template_id.expect("expected remaining shadowroot template");
    assert!(
      doc.node(inert_template_id).inert_subtree,
      "unpromoted <template shadowroot> siblings must remain inert"
    );

    let shadow_span = find_node_by_id_attribute(&doc, "shadow").expect("expected shadow span");
    let inert_span = find_node_by_id_attribute(&doc, "inert").expect("expected inert span");
    assert!(
      !doc.is_descendant_of_inert_template(shadow_span),
      "shadow root descendants must not be treated as template-inert"
    );
    assert!(
      doc.is_descendant_of_inert_template(inert_span),
      "unpromoted shadowroot template contents must remain inert"
    );
  }
}

#[cfg(test)]
mod document_fragment_tests;
