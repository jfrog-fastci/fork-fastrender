use crate::dom::HTML_NAMESPACE;
use crate::html::base_url_tracker::BaseUrlTracker;

use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode as HtmlQuirksMode, TreeSink};
use html5ever::tendril::StrTendril;
use markup5ever::interface::{Attribute, ExpandedName};
use markup5ever::QualName;
use rustc_hash::FxHashSet;
use selectors::context::QuirksMode as SelectorQuirksMode;
use std::borrow::Cow;
use std::cell::{Ref, RefCell, UnsafeCell};
use std::rc::Rc;

use super::{Document, NodeId, NodeKind};

/// html5ever [`TreeSink`] that incrementally builds a live [`dom2::Document`].
///
/// This is intentionally minimal and currently mirrors the renderer DOM conversion:
/// - Comments / processing instructions / doctypes are ignored (not inserted into the tree).
/// - HTML namespace elements store `namespace=""` for compatibility with existing selector logic.
///
/// The sink also performs parse-time `<base href>` tracking via a shared [`BaseUrlTracker`].
pub struct Dom2TreeSink {
  document: RefCell<Document>,
  base_url: Rc<RefCell<BaseUrlTracker>>,
  /// Stable element names used by html5ever during parsing.
  ///
  /// `TreeSink::elem_name` returns an [`ExpandedName`] that borrows from a boxed [`QualName`]. Names
  /// are boxed so references remain valid even if the backing vector reallocates while parsing.
  ///
  /// `None` entries correspond to non-element nodes.
  names: UnsafeCell<Vec<Option<Box<QualName>>>>,
  /// Nodes created for markup types that FastRender currently discards (comments / PIs).
  ///
  /// These nodes are kept disconnected so they are not traversed from the document root.
  ignored: RefCell<FxHashSet<NodeId>>,
}

impl Dom2TreeSink {
  pub fn new(base_url: Rc<RefCell<BaseUrlTracker>>) -> Self {
    let document = RefCell::new(Document::new(SelectorQuirksMode::NoQuirks));
    // Root document node has no element name. `UnsafeCell` allows `elem_name` to return borrowed
    // references without tying them to a `RefCell` guard.
    let names = UnsafeCell::new(vec![None]);
    Self {
      document,
      base_url,
      names,
      ignored: RefCell::new(FxHashSet::default()),
    }
  }

  pub fn document(&self) -> Ref<'_, Document> {
    self.document.borrow()
  }

  fn is_ignored(&self, id: NodeId) -> bool {
    self.ignored.borrow().contains(&id)
  }

  fn ensure_names_len(&self) {
    let doc_len = self.document.borrow().nodes_len();
    // Safety: `names` is only mutated by this sink during parsing.
    let names_len = unsafe { &*self.names.get() }.len();
    debug_assert_eq!(names_len, doc_len, "Dom2TreeSink name table out of sync with document nodes");
  }

  fn detach_with_doc(doc: &mut Document, target: NodeId) {
    let parent = doc.node(target).parent;
    let Some(parent_id) = parent else {
      return;
    };

    let parent_node = doc.node_mut(parent_id);
    if let Some(pos) = parent_node.children.iter().position(|&c| c == target) {
      parent_node.children.remove(pos);
    }
    doc.node_mut(target).parent = None;
  }

  fn append_existing_child(&self, parent: NodeId, child: NodeId) {
    if self.is_ignored(child) {
      return;
    }

    {
      let mut doc = self.document.borrow_mut();
      Self::detach_with_doc(&mut doc, child);
      doc.node_mut(child).parent = Some(parent);
      doc.node_mut(parent).children.push(child);
    }

    self.on_element_inserted(child);
  }

  fn insert_existing_child_before(&self, parent: NodeId, sibling: NodeId, child: NodeId) {
    if self.is_ignored(child) {
      return;
    }

    {
      let mut doc = self.document.borrow_mut();
      Self::detach_with_doc(&mut doc, child);
      let insert_pos = doc
        .node(parent)
        .children
        .iter()
        .position(|&c| c == sibling)
        .unwrap_or_else(|| doc.node(parent).children.len());

      let parent_node = doc.node_mut(parent);
      parent_node.children.insert(insert_pos, child);
      doc.node_mut(child).parent = Some(parent);
    }

    self.on_element_inserted(child);
  }

  fn append_text(&self, parent: NodeId, text: StrTendril) {
    if text.is_empty() {
      return;
    }

    let mut doc = self.document.borrow_mut();
    if let Some(&last) = doc.node(parent).children.last() {
      if let NodeKind::Text { content } = &mut doc.node_mut(last).kind {
        content.push_str(&text);
        return;
      }
    }

    let id = doc.push_node(NodeKind::Text { content: text.to_string() }, Some(parent), false);
    // Safety: `names` is only mutated by this sink during parsing.
    unsafe { &mut *self.names.get() }.push(None);
    drop(doc);
    self.ensure_names_len();
    let _ = id;
  }

  fn insert_text_before(&self, parent: NodeId, sibling: NodeId, text: StrTendril) {
    if text.is_empty() {
      return;
    }

    let mut doc = self.document.borrow_mut();
    let insert_pos = doc
      .node(parent)
      .children
      .iter()
      .position(|&c| c == sibling)
      .unwrap_or_else(|| doc.node(parent).children.len());

    // Merge with an immediately preceding text node when possible.
    if insert_pos > 0 {
      let prev = doc.node(parent).children[insert_pos - 1];
      if let NodeKind::Text { content } = &mut doc.node_mut(prev).kind {
        content.push_str(&text);
        return;
      }
    }

    let id = doc.push_node(NodeKind::Text { content: text.to_string() }, None, false);
    // Safety: `names` is only mutated by this sink during parsing.
    unsafe { &mut *self.names.get() }.push(None);
    Self::detach_with_doc(&mut doc, id);
    doc.node_mut(id).parent = Some(parent);
    doc.node_mut(parent).children.insert(insert_pos, id);
    drop(doc);
    self.ensure_names_len();
  }

  fn insertion_context_flags(doc: &Document, parent: NodeId) -> (bool, bool, bool) {
    let mut in_head = false;
    let mut in_foreign_namespace = false;
    let mut in_template = false;

    let mut current = Some(parent);
    // Defensive bound: prevent infinite loops if the tree becomes corrupted.
    let mut remaining = doc.nodes_len() + 1;
    while let Some(id) = current {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      match &doc.node(id).kind {
        NodeKind::Element {
          tag_name,
          namespace,
          ..
        } => {
          if tag_name.eq_ignore_ascii_case("head")
            && (namespace.is_empty() || namespace == HTML_NAMESPACE)
          {
            in_head = true;
          }
          if tag_name.eq_ignore_ascii_case("template") {
            in_template = true;
          }
          if !(namespace.is_empty() || namespace == HTML_NAMESPACE) {
            in_foreign_namespace = true;
          }
        }
        NodeKind::Slot { namespace, .. } => {
          if !(namespace.is_empty() || namespace == HTML_NAMESPACE) {
            in_foreign_namespace = true;
          }
        }
        NodeKind::Document { .. } | NodeKind::ShadowRoot { .. } | NodeKind::Text { .. } => {}
      }

      current = doc.node(id).parent;
    }

    (in_head, in_foreign_namespace, in_template)
  }

  fn on_element_inserted(&self, node: NodeId) {
    let doc = self.document.borrow();
    let NodeKind::Element {
      tag_name,
      namespace,
      attributes,
    } = &doc.node(node).kind
    else {
      return;
    };

    let Some(parent) = doc.node(node).parent else {
      return;
    };

    let (in_head, in_foreign_namespace, in_template) = Self::insertion_context_flags(&doc, parent);
    self.base_url.borrow_mut().on_element_inserted(
      tag_name,
      namespace,
      attributes,
      in_head,
      in_foreign_namespace,
      in_template,
    );
  }
}

impl TreeSink for Dom2TreeSink {
  type Handle = NodeId;
  type Output = Document;
  type ElemName<'a> = ExpandedName<'a>;

  fn finish(self) -> Document {
    self.document.into_inner()
  }

  fn parse_error(&self, _msg: Cow<'static, str>) {}

  fn get_document(&self) -> NodeId {
    self.document.borrow().root()
  }

  fn set_quirks_mode(&self, mode: HtmlQuirksMode) {
    let quirks_mode = match mode {
      HtmlQuirksMode::Quirks => SelectorQuirksMode::Quirks,
      HtmlQuirksMode::LimitedQuirks => SelectorQuirksMode::LimitedQuirks,
      HtmlQuirksMode::NoQuirks => SelectorQuirksMode::NoQuirks,
    };
    let mut doc = self.document.borrow_mut();
    let root = doc.root();
    if let NodeKind::Document { quirks_mode: q } = &mut doc.node_mut(root).kind {
      *q = quirks_mode;
    }
  }

  fn elem_name<'a>(&'a self, target: &'a NodeId) -> ExpandedName<'a> {
    // Safety: `names` is only mutated by this sink during parsing, and `QualName` values are boxed
    // so references remain valid even if the backing vector reallocates.
    let names = unsafe { &*self.names.get() };
    let Some(name) = names
      .get(target.index())
      .and_then(|v| v.as_ref().map(|b| b.as_ref()))
    else {
      panic!("Dom2TreeSink::elem_name called for non-element node: {target:?}");
    };
    ExpandedName {
      ns: &name.ns,
      local: &name.local,
    }
  }

  fn create_element(&self, name: QualName, attrs: Vec<Attribute>, _flags: ElementFlags) -> NodeId {
    let namespace = if name.ns.as_ref() == HTML_NAMESPACE {
      String::new()
    } else {
      name.ns.to_string()
    };
    let tag_name = name.local.to_string();

    let mut attributes: Vec<(String, String)> = Vec::with_capacity(attrs.len());
    for attr in attrs {
      attributes.push((attr.name.local.to_string(), attr.value.to_string()));
    }

    let inert_subtree = tag_name.eq_ignore_ascii_case("template");

    let is_html_slot = tag_name.eq_ignore_ascii_case("slot")
      && (namespace.is_empty() || namespace == HTML_NAMESPACE);
    let kind = if is_html_slot {
      NodeKind::Slot {
        namespace,
        attributes,
        assigned: false,
      }
    } else {
      NodeKind::Element {
        tag_name,
        namespace,
        attributes,
      }
    };

    let mut doc = self.document.borrow_mut();
    let id = doc.push_node(kind, None, inert_subtree);
    // Safety: `names` is only mutated by this sink during parsing.
    unsafe { &mut *self.names.get() }.push(Some(Box::new(name)));
    drop(doc);
    self.ensure_names_len();
    id
  }

  fn create_comment(&self, _text: StrTendril) -> NodeId {
    // FastRender's immutable DOM representation drops comments. Keep the node disconnected.
    let mut doc = self.document.borrow_mut();
    let id = doc.push_node(NodeKind::Text { content: String::new() }, None, false);
    // Safety: `names` is only mutated by this sink during parsing.
    unsafe { &mut *self.names.get() }.push(None);
    drop(doc);
    self.ignored.borrow_mut().insert(id);
    self.ensure_names_len();
    id
  }

  fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
    // FastRender drops processing instructions; keep disconnected.
    let mut doc = self.document.borrow_mut();
    let id = doc.push_node(NodeKind::Text { content: String::new() }, None, false);
    // Safety: `names` is only mutated by this sink during parsing.
    unsafe { &mut *self.names.get() }.push(None);
    drop(doc);
    self.ignored.borrow_mut().insert(id);
    self.ensure_names_len();
    id
  }

  fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
    match child {
      NodeOrText::AppendNode(node) => self.append_existing_child(*parent, node),
      NodeOrText::AppendText(text) => self.append_text(*parent, text),
    }
  }

  fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
    let parent = {
      let doc = self.document.borrow();
      doc.node(*sibling).parent
    };
    let Some(parent) = parent else {
      return;
    };

    match child {
      NodeOrText::AppendNode(node) => self.insert_existing_child_before(parent, *sibling, node),
      NodeOrText::AppendText(text) => self.insert_text_before(parent, *sibling, text),
    }
  }

  fn append_based_on_parent_node(
    &self,
    element: &NodeId,
    prev_element: &NodeId,
    child: NodeOrText<NodeId>,
  ) {
    let parent = {
      let doc = self.document.borrow();
      doc.node(*element).parent
    };
    if let Some(parent) = parent {
      // Prefer inserting before `prev_element` when it shares the same parent (foster parenting).
      let insert_before = {
        let doc = self.document.borrow();
        if doc.node(*prev_element).parent == Some(parent) {
          *prev_element
        } else {
          *element
        }
      };
      self.append_before_sibling(&insert_before, child);
    } else {
      self.append(element, child);
    }
  }

  fn remove_from_parent(&self, target: &NodeId) {
    let mut doc = self.document.borrow_mut();
    Self::detach_with_doc(&mut doc, *target);
  }

  fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
    let mut doc = self.document.borrow_mut();
    let children = std::mem::take(&mut doc.node_mut(*node).children);
    for child in children {
      Self::detach_with_doc(&mut doc, child);
      doc.node_mut(child).parent = Some(*new_parent);
      doc.node_mut(*new_parent).children.push(child);
    }
  }

  fn mark_script_already_started(&self, _node: &NodeId) {}

  fn pop(&self, _node: &NodeId) {}

  fn get_template_contents(&self, target: &NodeId) -> NodeId {
    // FastRender represents template contents as children of the `<template>` element itself.
    *target
  }

  fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
    x == y
  }

  fn append_doctype_to_document(&self, _name: StrTendril, _public_id: StrTendril, _system_id: StrTendril) {}

  fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
    let mut doc = self.document.borrow_mut();
    let node = doc.node_mut(*target);
    let Some(existing) = (match &mut node.kind {
      NodeKind::Element { attributes, .. } => Some(attributes),
      NodeKind::Slot { attributes, .. } => Some(attributes),
      _ => None,
    }) else {
      return;
    };

    for attr in attrs {
      let name = attr.name.local.to_string();
      if existing.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
        continue;
      }
      existing.push((name, attr.value.to_string()));
    }
  }
}
