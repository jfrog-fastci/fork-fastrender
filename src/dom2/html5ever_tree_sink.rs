use crate::dom::HTML_NAMESPACE;
use crate::html::base_url_tracker::BaseUrlTracker;

use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode as HtmlQuirksMode, TreeSink};
use markup5ever::interface::tree_builder::ElemName;
use markup5ever::interface::Attribute;
use markup5ever::{LocalName, Namespace, QualName};
use rustc_hash::FxHashSet;
use selectors::context::QuirksMode as SelectorQuirksMode;
use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::rc::Rc;

use super::{Document, NodeId, NodeKind};

const INVALID: NodeId = NodeId(usize::MAX);

#[derive(Debug, Clone)]
pub struct Dom2ElemName {
  ns: Namespace,
  local: LocalName,
}

impl ElemName for Dom2ElemName {
  fn ns(&self) -> &Namespace {
    &self.ns
  }

  fn local_name(&self) -> &LocalName {
    &self.local
  }
}

#[derive(Default)]
struct SinkState {
  /// Merge barrier between a node and its previous sibling.
  ///
  /// This is used to preserve text split points around ignored markup (comments / PI) without
  /// keeping those nodes in the DOM.
  merge_barrier_before: FxHashSet<NodeId>,
  /// Merge barrier at the end of a parent's children list.
  merge_barrier_at_end: FxHashSet<NodeId>,
}

/// html5ever [`TreeSink`] that incrementally builds a live [`dom2::Document`].
///
/// Notes:
/// - Comments / processing instructions / doctypes are ignored (not inserted into the tree).
/// - HTML namespace elements store `namespace=""` for compatibility with existing selector logic.
/// - The sink performs parse-time `<base href>` tracking via a shared [`BaseUrlTracker`].
pub struct Dom2TreeSink {
  document: RefCell<Document>,
  base_url: Rc<RefCell<BaseUrlTracker>>,
  state: RefCell<SinkState>,
}

impl Dom2TreeSink {
  pub fn new(base_url: Rc<RefCell<BaseUrlTracker>>) -> Self {
    Self {
      document: RefCell::new(Document::new(SelectorQuirksMode::NoQuirks)),
      base_url,
      state: RefCell::new(SinkState::default()),
    }
  }

  pub fn document(&self) -> Ref<'_, Document> {
    self.document.borrow()
  }

  fn map_quirks_mode(mode: HtmlQuirksMode) -> SelectorQuirksMode {
    match mode {
      HtmlQuirksMode::Quirks => SelectorQuirksMode::Quirks,
      HtmlQuirksMode::LimitedQuirks => SelectorQuirksMode::LimitedQuirks,
      HtmlQuirksMode::NoQuirks => SelectorQuirksMode::NoQuirks,
    }
  }

  fn is_html_namespace(namespace: &str) -> bool {
    namespace.is_empty() || namespace == HTML_NAMESPACE
  }

  fn normalize_namespace_for_storage(ns: &str) -> String {
    if ns == HTML_NAMESPACE {
      String::new()
    } else {
      ns.to_string()
    }
  }

  fn compute_insertion_flags(doc: &Document, parent: NodeId) -> (bool, bool, bool) {
    let mut in_head = false;
    let mut in_foreign_namespace = false;
    let mut in_template = false;

    let mut current = Some(parent);
    // Defensive bound to avoid infinite loops if the tree becomes corrupted.
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
          if tag_name.eq_ignore_ascii_case("template") {
            in_template = true;
          }
          if tag_name.eq_ignore_ascii_case("head") && Self::is_html_namespace(namespace) {
            in_head = true;
          }
          if !Self::is_html_namespace(namespace) {
            in_foreign_namespace = true;
          }
        }
        NodeKind::Slot { namespace, .. } => {
          if !Self::is_html_namespace(namespace) {
            in_foreign_namespace = true;
          }
        }
        NodeKind::Document { .. } | NodeKind::ShadowRoot { .. } | NodeKind::Text { .. } => {}
      }
      current = doc.node(id).parent;
    }

    (in_head, in_foreign_namespace, in_template)
  }

  fn element_info<'a>(
    kind: &'a NodeKind,
  ) -> Option<(&'a str, &'a str, &'a [(String, String)])> {
    match kind {
      NodeKind::Element {
        tag_name,
        namespace,
        attributes,
      } => Some((tag_name.as_str(), namespace.as_str(), attributes.as_slice())),
      NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => Some(("slot", namespace.as_str(), attributes.as_slice())),
      _ => None,
    }
  }

  fn detach_from_parent(state: &mut SinkState, doc: &mut Document, target: NodeId) {
    let Some(old_parent) = doc.node(target).parent else {
      return;
    };
    let Some(pos) = doc.node(old_parent).children.iter().position(|&c| c == target) else {
      doc.node_mut(target).parent = None;
      return;
    };

    // If there was a merge barrier directly before this node, move it to the next sibling (or the
    // end of the parent) to mimic the ignored node remaining in-place.
    if state.merge_barrier_before.remove(&target) {
      if let Some(next) = doc.node(old_parent).children.get(pos + 1).copied() {
        state.merge_barrier_before.insert(next);
      } else {
        state.merge_barrier_at_end.insert(old_parent);
      }
    }

    doc.node_mut(old_parent).children.remove(pos);
    doc.node_mut(target).parent = None;
  }

  fn insert_node_before(
    state: &mut SinkState,
    doc: &mut Document,
    parent: NodeId,
    reference: Option<NodeId>,
    child: NodeId,
  ) -> bool {
    if child == INVALID || parent == INVALID {
      return false;
    }

    if doc.node(child).parent.is_some() {
      Self::detach_from_parent(state, doc, child);
    }

    let insertion_idx = match reference {
      Some(reference) => doc.node(parent).children.iter().position(|&c| c == reference),
      None => Some(doc.node(parent).children.len()),
    };
    let Some(insertion_idx) = insertion_idx else {
      return false;
    };

    // If a merge barrier was at the insertion point, move it to before the inserted node.
    if reference.is_none() && state.merge_barrier_at_end.remove(&parent) {
      state.merge_barrier_before.insert(child);
    } else if let Some(reference) = reference {
      if state.merge_barrier_before.remove(&reference) {
        state.merge_barrier_before.insert(child);
      }
    }

    // Avoid inserting the same node twice.
    if doc.node(parent).children.get(insertion_idx).copied() == Some(child) {
      return false;
    }

    doc.node_mut(parent).children.insert(insertion_idx, child);
    doc.node_mut(child).parent = Some(parent);
    true
  }

  fn append_text_at(
    state: &mut SinkState,
    doc: &mut Document,
    parent: NodeId,
    reference: Option<NodeId>,
    text: &str,
  ) {
    if text.is_empty() {
      return;
    }

    if reference.is_none() {
      let blocked_by_barrier = state.merge_barrier_at_end.contains(&parent);
      if !blocked_by_barrier {
        if let Some(last_child) = doc.node(parent).children.last().copied() {
          if let NodeKind::Text { content } = &mut doc.node_mut(last_child).kind {
            content.push_str(text);
            return;
          }
        }
      }

      let text_id = doc.push_node(
        NodeKind::Text {
          content: text.to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );

      // A barrier at end now becomes a barrier before the newly appended node.
      if state.merge_barrier_at_end.remove(&parent) {
        state.merge_barrier_before.insert(text_id);
      }

      return;
    }

    let reference = reference.unwrap();
    let Some(insert_pos) = doc.node(parent).children.iter().position(|&c| c == reference) else {
      return;
    };

    let barrier = state.merge_barrier_before.contains(&reference);
    let prev = if insert_pos == 0 {
      None
    } else {
      doc.node(parent).children.get(insert_pos - 1).copied()
    };

    let can_merge_prev = !barrier
      && prev.is_some_and(|id| matches!(&doc.node(id).kind, NodeKind::Text { .. }));
    let can_merge_next = matches!(&doc.node(reference).kind, NodeKind::Text { .. });

    if can_merge_prev {
      let prev_id = prev.unwrap();
      if let NodeKind::Text { content } = &mut doc.node_mut(prev_id).kind {
        content.push_str(text);
      }

      // If the next sibling is also text, merge it too to avoid adjacent text nodes.
      if can_merge_next {
        let next_content = match &doc.node(reference).kind {
          NodeKind::Text { content } => content.clone(),
          _ => String::new(),
        };
        if let NodeKind::Text { content } = &mut doc.node_mut(prev_id).kind {
          content.push_str(&next_content);
        }
        Self::detach_from_parent(state, doc, reference);
      }

      return;
    }

    if can_merge_next {
      if let NodeKind::Text { content } = &mut doc.node_mut(reference).kind {
        content.insert_str(0, text);
      }
      return;
    }

    let text_id = doc.push_node(
      NodeKind::Text {
        content: text.to_string(),
      },
      None,
      /* inert_subtree */ false,
    );
    doc.node_mut(text_id).parent = Some(parent);
    doc.node_mut(parent).children.insert(insert_pos, text_id);

    // Preserve barrier-at-reference semantics by moving it to the new node.
    if state.merge_barrier_before.remove(&reference) {
      state.merge_barrier_before.insert(text_id);
    }
  }

  fn note_element_inserted(&self, doc: &Document, parent: NodeId, child: NodeId) {
    let Some((tag_name, namespace, attrs)) = Self::element_info(&doc.node(child).kind) else {
      return;
    };
    let (in_head, in_foreign_namespace, in_template) = Self::compute_insertion_flags(doc, parent);
    self.base_url.borrow_mut().on_element_inserted(
      tag_name,
      namespace,
      attrs,
      in_head,
      in_foreign_namespace,
      in_template,
    );
  }
}

impl TreeSink for Dom2TreeSink {
  type Handle = NodeId;
  type Output = Document;
  type ElemName<'a>
    = Dom2ElemName
  where
    Self: 'a;

  fn finish(self) -> Document {
    self.document.into_inner()
  }

  fn parse_error(&self, _msg: Cow<'static, str>) {}

  fn get_document(&self) -> NodeId {
    self.document.borrow().root()
  }

  fn set_quirks_mode(&self, mode: HtmlQuirksMode) {
    let quirks_mode = Self::map_quirks_mode(mode);
    let mut doc = self.document.borrow_mut();
    let root = doc.root();
    if let NodeKind::Document { quirks_mode: q } = &mut doc.node_mut(root).kind {
      *q = quirks_mode;
    }
  }

  fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
    x == y
  }

  fn elem_name(&self, target: &NodeId) -> Dom2ElemName {
    if *target == INVALID {
      return Dom2ElemName {
        ns: Namespace::from(HTML_NAMESPACE),
        local: LocalName::from(""),
      };
    }

    let doc = self.document.borrow();
    match &doc.node(*target).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        let ns = if namespace.is_empty() {
          Namespace::from(HTML_NAMESPACE)
        } else {
          Namespace::from(namespace.as_str())
        };
        Dom2ElemName {
          ns,
          local: LocalName::from(tag_name.as_str()),
        }
      }
      NodeKind::Slot { namespace, .. } => {
        let ns = if namespace.is_empty() {
          Namespace::from(HTML_NAMESPACE)
        } else {
          Namespace::from(namespace.as_str())
        };
        Dom2ElemName {
          ns,
          local: LocalName::from("slot"),
        }
      }
      _ => Dom2ElemName {
        ns: Namespace::from(HTML_NAMESPACE),
        local: LocalName::from(""),
      },
    }
  }

  fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
    let namespace = Self::normalize_namespace_for_storage(name.ns.as_ref());
    let mut attributes = Vec::with_capacity(attrs.len());
    for attr in attrs {
      attributes.push((attr.name.local.to_string(), attr.value.to_string()));
    }

    let is_html_slot = name.local.as_ref().eq_ignore_ascii_case("slot")
      && Self::is_html_namespace(namespace.as_str());

    let inert_subtree = name.local.as_ref().eq_ignore_ascii_case("template");
    let kind = if is_html_slot {
      NodeKind::Slot {
        namespace,
        attributes,
        assigned: false,
      }
    } else {
      NodeKind::Element {
        tag_name: name.local.to_string(),
        namespace,
        attributes,
      }
    };

    let mut doc = self.document.borrow_mut();
    let id = doc.push_node(kind, None, inert_subtree);
    doc.node_mut(id).mathml_annotation_xml_integration_point = flags.mathml_annotation_xml_integration_point;
    id
  }

  fn create_comment(&self, _text: StrTendril) -> NodeId {
    INVALID
  }

  fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
    INVALID
  }

  fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
    if *parent == INVALID {
      return;
    }

    match child {
      NodeOrText::AppendText(text) => {
        let mut doc = self.document.borrow_mut();
        let mut state = self.state.borrow_mut();
        Self::append_text_at(&mut state, &mut doc, *parent, None, text.as_ref());
      }
      NodeOrText::AppendNode(node) => {
        if node == INVALID {
          self.state.borrow_mut().merge_barrier_at_end.insert(*parent);
          return;
        }

        let mut doc = self.document.borrow_mut();
        let mut state = self.state.borrow_mut();
        let was_unparented = doc.node(node).parent.is_none();
        if Self::insert_node_before(&mut state, &mut doc, *parent, None, node) && was_unparented {
          self.note_element_inserted(&doc, *parent, node);
        }
      }
    }
  }

  fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
    if *sibling == INVALID {
      return;
    }

    let parent = {
      let doc = self.document.borrow();
      doc.node(*sibling).parent
    };
    let Some(parent) = parent else {
      return;
    };

    match child {
      NodeOrText::AppendText(text) => {
        let mut doc = self.document.borrow_mut();
        let mut state = self.state.borrow_mut();
        Self::append_text_at(&mut state, &mut doc, parent, Some(*sibling), text.as_ref());
      }
      NodeOrText::AppendNode(node) => {
        if node == INVALID {
          self.state.borrow_mut().merge_barrier_before.insert(*sibling);
          return;
        }

        let mut doc = self.document.borrow_mut();
        let mut state = self.state.borrow_mut();
        let was_unparented = doc.node(node).parent.is_none();
        if Self::insert_node_before(&mut state, &mut doc, parent, Some(*sibling), node)
          && was_unparented
        {
          self.note_element_inserted(&doc, parent, node);
        }
      }
    }
  }

  fn append_based_on_parent_node(&self, element: &NodeId, prev_element: &NodeId, child: NodeOrText<NodeId>) {
    if *element != INVALID {
      let parent = {
        let doc = self.document.borrow();
        doc.node(*element).parent
      };
      if parent.is_some() {
        self.append_before_sibling(element, child);
        return;
      }
    }

    self.append(prev_element, child);
  }

  fn append_doctype_to_document(&self, _name: StrTendril, _public_id: StrTendril, _system_id: StrTendril) {}

  fn get_template_contents(&self, target: &NodeId) -> NodeId {
    // FastRender represents template contents as children of the `<template>` element itself.
    *target
  }

  fn remove_from_parent(&self, target: &NodeId) {
    if *target == INVALID {
      return;
    }
    let mut doc = self.document.borrow_mut();
    let mut state = self.state.borrow_mut();
    Self::detach_from_parent(&mut state, &mut doc, *target);
  }

  fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
    if *node == INVALID || *new_parent == INVALID {
      return;
    }

    let mut doc = self.document.borrow_mut();
    let mut state = self.state.borrow_mut();
    let moved_children = std::mem::take(&mut doc.node_mut(*node).children);

    if !moved_children.is_empty() && state.merge_barrier_at_end.remove(new_parent) {
      state.merge_barrier_before.insert(moved_children[0]);
    }

    for child in moved_children.iter().copied() {
      doc.node_mut(child).parent = Some(*new_parent);
    }
    doc.node_mut(*new_parent).children.extend(moved_children.iter().copied());

    if state.merge_barrier_at_end.remove(node) {
      state.merge_barrier_at_end.insert(*new_parent);
    }
  }

  fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
    if *target == INVALID {
      return;
    }

    let mut doc = self.document.borrow_mut();
    let kind = &mut doc.node_mut(*target).kind;
    let (existing, is_html) = match kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      } => (attributes, Self::is_html_namespace(namespace)),
      NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => (attributes, Self::is_html_namespace(namespace)),
      _ => return,
    };

    for attr in attrs {
      let name = attr.name.local.to_string();
      let present = existing.iter().any(|(k, _)| {
        if is_html {
          k.eq_ignore_ascii_case(name.as_str())
        } else {
          k == &name
        }
      });
      if present {
        continue;
      }
      existing.push((name, attr.value.to_string()));
    }
  }

  fn mark_script_already_started(&self, node: &NodeId) {
    if *node == INVALID {
      return;
    }
    self.document.borrow_mut().node_mut(*node).script_already_started = true;
  }

  fn is_mathml_annotation_xml_integration_point(&self, node: &NodeId) -> bool {
    if *node == INVALID {
      return false;
    }
    self
      .document
      .borrow()
      .node(*node)
      .mathml_annotation_xml_integration_point
  }

  fn pop(&self, _node: &NodeId) {}
}

#[cfg(test)]
mod tests {
  use super::Dom2TreeSink;
  use crate::debug::snapshot::snapshot_dom;
  use crate::dom2::{NodeId, NodeKind};
  use crate::html::base_url_tracker::BaseUrlTracker;
  use html5ever::tendril::TendrilSink;
  use html5ever::ParseOpts;
  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::rc::Rc;

  fn parse_with_sink(html: &str) -> crate::dom2::Document {
    let base_url = Rc::new(RefCell::new(BaseUrlTracker::new(None)));
    let sink = Dom2TreeSink::new(base_url);
    html5ever::parse_document(sink, ParseOpts::default()).one(html)
  }

  #[test]
  fn template_contents_preserved_and_inert_subtree_set() {
    let html = concat!(
      "<!doctype html>",
      "<html><body><template><span>in</span></template><div>out</div></body></html>"
    );
    let expected = crate::dom::parse_html(html).unwrap();

    let doc = parse_with_sink(html);
    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));

    let template_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("template") => Some(NodeId(idx)),
        _ => None,
      })
      .expect("template element not found");
    assert!(doc.node(template_id).inert_subtree);
    assert!(
      !doc.node(template_id).children.is_empty(),
      "template contents should remain attached"
    );
  }

  #[test]
  fn foster_parenting_matches_renderer_dom() {
    let html = "<!doctype html><table>foo<tr><td>bar</td></tr></table>";
    let expected = crate::dom::parse_html(html).unwrap();

    let doc = parse_with_sink(html);
    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));
  }

  #[test]
  fn sets_document_quirks_mode() {
    let html_no_quirks = "<!doctype html><p>x</p>";
    let html_quirks = "<!doctype html public \"-//W3C//DTD HTML 3.2 Final//EN\"><p>x</p>";

    let expected_no_quirks = crate::dom::parse_html(html_no_quirks).unwrap();
    let expected_quirks = crate::dom::parse_html(html_quirks).unwrap();

    let doc_no_quirks = parse_with_sink(html_no_quirks);
    let doc_quirks = parse_with_sink(html_quirks);

    assert_eq!(
      expected_no_quirks.document_quirks_mode(),
      doc_no_quirks.to_renderer_dom().document_quirks_mode()
    );
    assert_eq!(
      expected_quirks.document_quirks_mode(),
      doc_quirks.to_renderer_dom().document_quirks_mode()
    );

    assert_eq!(
      doc_no_quirks.node(doc_no_quirks.root()).kind.clone(),
      NodeKind::Document {
        quirks_mode: QuirksMode::NoQuirks
      }
    );
    assert_eq!(
      doc_quirks.node(doc_quirks.root()).kind.clone(),
      NodeKind::Document {
        quirks_mode: QuirksMode::Quirks
      }
    );
  }

  #[test]
  fn merges_adjacent_text_insertions() {
    let base_url = Rc::new(RefCell::new(BaseUrlTracker::new(None)));
    let sink = Dom2TreeSink::new(base_url);
    let mut parser = html5ever::parse_document(sink, ParseOpts::default());
    parser.process("<p>".into());
    parser.process("a".into());
    parser.process("b</p>".into());
    let doc = parser.finish();

    let p_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("p") => Some(NodeId(idx)),
        _ => None,
      })
      .expect("<p> not found");

    let text_children: Vec<String> = doc
      .node(p_id)
      .children
      .iter()
      .filter_map(|&id| match &doc.node(id).kind {
        NodeKind::Text { content } => Some(content.clone()),
        _ => None,
      })
      .collect();
    assert_eq!(text_children, vec!["ab".to_string()]);
  }

  #[test]
  fn ignored_comment_prevents_text_merge_across_boundary() {
    let html = "<!doctype html><div>a<!--c-->b</div>";
    let expected = crate::dom::parse_html(html).unwrap();

    let doc = parse_with_sink(html);
    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));

    let div_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("div") => Some(NodeId(idx)),
        _ => None,
      })
      .expect("<div> not found");
    let text_nodes: Vec<String> = doc
      .node(div_id)
      .children
      .iter()
      .filter_map(|&id| match &doc.node(id).kind {
        NodeKind::Text { content } => Some(content.clone()),
        _ => None,
      })
      .collect();
    assert_eq!(text_nodes, vec!["a".to_string(), "b".to_string()]);
  }

  #[test]
  fn base_url_tracker_ignores_template_base_and_applies_head_base() {
    let html = concat!(
      "<!doctype html>",
      "<html><head>",
      "<template><base href=\"https://bad-template.example/\"></template>",
      "<base href=\"https://good.example/base/\">",
      "</head><body>",
      "<base href=\"https://bad-body.example/\">",
      "</body></html>",
    );

    let base_url = Rc::new(RefCell::new(BaseUrlTracker::new(None)));
    let sink = Dom2TreeSink::new(Rc::clone(&base_url));
    let _doc = html5ever::parse_document(sink, ParseOpts::default()).one(html);

    assert_eq!(
      base_url.borrow().current_base_url().as_deref(),
      Some("https://good.example/base/")
    );
  }
}
