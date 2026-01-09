use crate::dom::HTML_NAMESPACE;
use crate::html::base_url_tracker::BaseUrlTracker;

use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode as HtmlQuirksMode, TreeSink};
use markup5ever::interface::tree_builder::ElemName;
use markup5ever::interface::Attribute;
use markup5ever::{LocalName, Namespace, QualName};
use selectors::context::QuirksMode as SelectorQuirksMode;
use std::borrow::Cow;
use std::cell::{Ref, RefCell, RefMut};

use super::{Document, NodeId, NodeKind};

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

/// html5ever [`TreeSink`] that incrementally builds a live [`dom2::Document`].
///
/// Notes:
/// - Comments / processing instructions / doctypes are stored in the `dom2` arena but are currently
///   ignored when snapshotting back into the renderer's immutable DOM representation (to match
///   `crate::dom::parse_html`).
/// - HTML namespace elements store `namespace=""` for compatibility with existing selector logic.
/// - The sink performs parse-time `<base href>` tracking via an internal [`BaseUrlTracker`].
pub struct Dom2TreeSink {
  document: RefCell<Document>,
  base_url_tracker: RefCell<BaseUrlTracker>,
}

impl Dom2TreeSink {
  pub fn new(document_url: Option<&str>) -> Self {
    Self {
      document: RefCell::new(Document::new(SelectorQuirksMode::NoQuirks)),
      base_url_tracker: RefCell::new(BaseUrlTracker::new(document_url)),
    }
  }

  pub fn document(&self) -> Ref<'_, Document> {
    self.document.borrow()
  }

  pub fn document_mut(&self) -> RefMut<'_, Document> {
    self.document.borrow_mut()
  }

  pub fn base_url_tracker(&self) -> Ref<'_, BaseUrlTracker> {
    self.base_url_tracker.borrow()
  }

  pub fn current_base_url(&self) -> Option<String> {
    self.base_url_tracker.borrow().current_base_url()
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

      if doc.node(id).inert_subtree {
        in_template = true;
      }

      match &doc.node(id).kind {
        NodeKind::Element {
          tag_name,
          namespace,
          ..
        } => {
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
        NodeKind::Document { .. }
        | NodeKind::Doctype { .. }
        | NodeKind::Comment { .. }
        | NodeKind::ProcessingInstruction { .. }
        | NodeKind::ShadowRoot { .. }
        | NodeKind::Text { .. } => {}
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

  fn detach_from_parent(doc: &mut Document, target: NodeId) {
    let Some(old_parent) = doc.node(target).parent else {
      return;
    };
    let Some(pos) = doc.node(old_parent).children.iter().position(|&c| c == target) else {
      doc.node_mut(target).parent = None;
      return;
    };

    doc.node_mut(old_parent).children.remove(pos);
    doc.node_mut(target).parent = None;
  }

  fn insert_node_before(
    doc: &mut Document,
    parent: NodeId,
    reference: Option<NodeId>,
    child: NodeId,
  ) -> bool {
    if doc.node(child).parent.is_some() {
      Self::detach_from_parent(doc, child);
    }

    let insertion_idx = match reference {
      Some(reference) => doc.node(parent).children.iter().position(|&c| c == reference),
      None => Some(doc.node(parent).children.len()),
    };
    let Some(insertion_idx) = insertion_idx else {
      return false;
    };

    // Avoid inserting the same node twice.
    if doc.node(parent).children.get(insertion_idx).copied() == Some(child) {
      return false;
    }

    doc.node_mut(parent).children.insert(insertion_idx, child);
    doc.node_mut(child).parent = Some(parent);
    true
  }

  fn append_text_at(
    doc: &mut Document,
    parent: NodeId,
    reference: Option<NodeId>,
    text: &str,
  ) {
    if text.is_empty() {
      return;
    }

    if reference.is_none() {
      if let Some(last_child) = doc.node(parent).children.last().copied() {
        if let NodeKind::Text { content } = &mut doc.node_mut(last_child).kind {
          content.push_str(text);
          return;
        }
      }

      doc.push_node(
        NodeKind::Text {
          content: text.to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );
      return;
    }

    let reference = reference.unwrap();
    let Some(insert_pos) = doc.node(parent).children.iter().position(|&c| c == reference) else {
      return;
    };
    let prev = if insert_pos == 0 {
      None
    } else {
      doc.node(parent).children.get(insert_pos - 1).copied()
    };

    let can_merge_prev = prev.is_some_and(|id| matches!(&doc.node(id).kind, NodeKind::Text { .. }));
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
        Self::detach_from_parent(doc, reference);
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
  }

  fn note_element_inserted(&self, doc: &Document, parent: NodeId, child: NodeId) {
    let Some((tag_name, namespace, attrs)) = Self::element_info(&doc.node(child).kind) else {
      return;
    };
    if !tag_name.eq_ignore_ascii_case("base") {
      return;
    }
    let (in_head, in_foreign_namespace, in_template) = Self::compute_insertion_flags(doc, parent);
    self
      .base_url_tracker
      .borrow_mut()
      .on_element_inserted(
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

  fn create_comment(&self, text: StrTendril) -> NodeId {
    self.document.borrow_mut().push_node(
      NodeKind::Comment {
        content: text.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
    self.document.borrow_mut().push_node(
      NodeKind::ProcessingInstruction {
        target: target.to_string(),
        data: data.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
    match child {
      NodeOrText::AppendText(text) => {
        let mut doc = self.document.borrow_mut();
        Self::append_text_at(&mut doc, *parent, None, text.as_ref());
      }
      NodeOrText::AppendNode(node) => {
        let mut doc = self.document.borrow_mut();
        if Self::insert_node_before(&mut doc, *parent, None, node) {
          self.note_element_inserted(&doc, *parent, node);
        }
      }
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
      NodeOrText::AppendText(text) => {
        let mut doc = self.document.borrow_mut();
        Self::append_text_at(&mut doc, parent, Some(*sibling), text.as_ref());
      }
      NodeOrText::AppendNode(node) => {
        let mut doc = self.document.borrow_mut();
        if Self::insert_node_before(&mut doc, parent, Some(*sibling), node) {
          self.note_element_inserted(&doc, parent, node);
        }
      }
    }
  }

  fn append_based_on_parent_node(&self, element: &NodeId, prev_element: &NodeId, child: NodeOrText<NodeId>) {
    let parent = {
      let doc = self.document.borrow();
      doc.node(*element).parent
    };
    if parent.is_some() {
      self.append_before_sibling(element, child);
      return;
    }
    self.append(prev_element, child)
  }

  fn append_doctype_to_document(&self, name: StrTendril, public_id: StrTendril, system_id: StrTendril) {
    let mut doc = self.document.borrow_mut();
    let root = doc.root();
    doc.push_node(
      NodeKind::Doctype {
        name: name.to_string(),
        public_id: public_id.to_string(),
        system_id: system_id.to_string(),
      },
      Some(root),
      /* inert_subtree */ false,
    );
  }

  fn get_template_contents(&self, target: &NodeId) -> NodeId {
    // FastRender represents template contents as children of the `<template>` element itself.
    *target
  }

  fn remove_from_parent(&self, target: &NodeId) {
    let mut doc = self.document.borrow_mut();
    Self::detach_from_parent(&mut doc, *target);
  }

  fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
    let mut doc = self.document.borrow_mut();
    let moved_children = std::mem::take(&mut doc.node_mut(*node).children);

    if moved_children.is_empty() {
      return;
    }

    let old_len = doc.node(*new_parent).children.len();
    for &child in &moved_children {
      doc.node_mut(child).parent = Some(*new_parent);
    }
    doc
      .node_mut(*new_parent)
      .children
      .extend(moved_children.iter().copied());

    // Merge boundary text nodes if reparenting created a new adjacency.
    if old_len > 0 {
      let prev = doc.node(*new_parent).children[old_len - 1];
      let next = doc.node(*new_parent).children[old_len];
      if matches!(&doc.node(prev).kind, NodeKind::Text { .. })
        && matches!(&doc.node(next).kind, NodeKind::Text { .. })
      {
        let next_content = match &mut doc.node_mut(next).kind {
          NodeKind::Text { content } => std::mem::take(content),
          _ => unreachable!("kind checked above"),
        };
        if let NodeKind::Text { content } = &mut doc.node_mut(prev).kind {
          content.push_str(&next_content);
        }
        Self::detach_from_parent(&mut doc, next);
      }
    }

    // Reparenting is another insertion point (e.g. foster parenting). Notify the base URL tracker
    // for any `<base>` moved into/out of `<head>` during parsing.
    for child in moved_children {
      self.note_element_inserted(&doc, *new_parent, child);
    }
  }

  fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
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
    self.document.borrow_mut().node_mut(*node).script_already_started = true;
  }

  fn is_mathml_annotation_xml_integration_point(&self, node: &NodeId) -> bool {
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
  use crate::dom2::{Document, NodeId, NodeKind};
  use html5ever::tendril::TendrilSink;
  use html5ever::ParseOpts;
  use selectors::context::QuirksMode;

  fn parse_with_sink(html: &str) -> crate::dom2::Document {
    let sink = Dom2TreeSink::new(None);
    html5ever::parse_document(sink, ParseOpts::default()).one(html)
  }

  fn assert_parent_child_invariants(doc: &Document) {
    for (idx, node) in doc.nodes().iter().enumerate() {
      let id = NodeId(idx);
      if id == doc.root() {
        assert!(node.parent.is_none(), "root node must have no parent");
      }
      if let Some(parent) = node.parent {
        assert!(
          doc.node(parent).children.contains(&id),
          "node parent pointer must be reflected in the parent's children list"
        );
      }
      for &child in &node.children {
        let child_node = doc.node(child);
        assert_eq!(
          child_node.parent,
          Some(id),
          "child must point back to parent"
        );
      }
    }
  }

  #[test]
  fn dom2_tree_sink_roundtrips_via_renderer_snapshot() {
    let html = concat!(
      "<!DOCTYPE html>",
      "<html><head><title>x</title></head>",
      "<body><div id=a class=b>Hello<span>world</span></div></body></html>"
    );
    let expected = crate::dom::parse_html(html).unwrap();

    let doc = parse_with_sink(html);
    assert_parent_child_invariants(&doc);

    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));
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
    let sink = Dom2TreeSink::new(None);
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
  fn wbr_snapshot_matches_legacy_parse_html() {
    let html = "<!doctype html><p>a<wbr>b</p>";
    let expected = crate::dom::parse_html(html).unwrap();

    let doc = parse_with_sink(html);
    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));
  }

  #[test]
  fn deep_tree_parses_without_recursion_overflow() {
    const DEPTH: usize = 50_000;

    let mut html = String::with_capacity(256 + DEPTH * 7);
    html.push_str("<!doctype html><html><head></head><body>");
    for _ in 0..DEPTH {
      html.push_str("<x>");
    }
    html.push_str("leaf");
    for _ in 0..DEPTH {
      html.push_str("</x>");
    }
    html.push_str("</body></html>");

    let doc = parse_with_sink(&html);
    assert_parent_child_invariants(&doc);

    assert!(
      doc.nodes_len() >= DEPTH + 5,
      "expected at least {} nodes, got {}",
      DEPTH + 5,
      doc.nodes_len()
    );
  }

  #[test]
  fn stores_doctype_and_comment_nodes_in_dom2() {
    let html = "<!doctype html><div><!--c--></div>";
    let expected = crate::dom::parse_html(html).unwrap();

    let doc = parse_with_sink(html);
    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));

    let doctype_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Doctype { .. } => Some(NodeId(idx)),
        _ => None,
      })
      .expect("doctype node not found");
    let NodeKind::Doctype { name, .. } = &doc.node(doctype_id).kind else {
      unreachable!("doctype_id must point at Doctype");
    };
    assert_eq!(name.to_ascii_lowercase(), "html");
    assert_eq!(doc.node(doctype_id).parent, Some(doc.root()));

    let div_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("div") => Some(NodeId(idx)),
        _ => None,
      })
      .expect("<div> not found");

    let comment_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Comment { .. } => Some(NodeId(idx)),
        _ => None,
      })
      .expect("comment node not found");
    let NodeKind::Comment { content } = &doc.node(comment_id).kind else {
      unreachable!("comment_id must point at Comment");
    };
    assert_eq!(content, "c");
    assert_eq!(doc.node(comment_id).parent, Some(div_id));
    assert!(
      doc.node(div_id).children.contains(&comment_id),
      "div should contain comment node"
    );
  }
}

#[cfg(test)]
mod base_url_tests {
  use super::Dom2TreeSink;
  use crate::dom2::{Document, NodeId, NodeKind};

  use html5ever::tendril::StrTendril;
  use html5ever::tokenizer::{BufferQueue, Tokenizer};
  use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
  use html5ever::ParseOpts;
  use html5ever::TokenizerResult;

  fn parse_with_base_url(html: &str) -> (Option<String>, Document) {
    let sink = Dom2TreeSink::new(Some("https://example.com/dir/page.html"));
    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: false,
        ..Default::default()
      },
      ..Default::default()
    };

    let tb = TreeBuilder::new(sink, opts.tree_builder);
    let mut tokenizer = Tokenizer::new(tb, opts.tokenizer);
    let mut input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(html));

    loop {
      match tokenizer.feed(&mut input) {
        TokenizerResult::Done => break,
        TokenizerResult::Script(_) => panic!("unexpected script pause with scripting disabled"),
      }
    }
    tokenizer.end();

    let doc = tokenizer.sink.sink.document.borrow().clone();
    let base_url = tokenizer.sink.sink.current_base_url();
    (base_url, doc)
  }

  fn parse_and_capture_base_url(html: &str) -> Option<String> {
    let (base_url, _doc) = parse_with_base_url(html);
    base_url
  }

  #[test]
  fn base_href_freezes_after_first_valid_base_in_head() {
    let html =
      "<!doctype html><html><head><base href=\"https://a/\"><base href=\"https://b/\"></head></html>";
    assert_eq!(parse_and_capture_base_url(html).as_deref(), Some("https://a/"));
  }

  #[test]
  fn base_in_template_in_head_is_ignored_and_does_not_freeze() {
    let html = concat!(
      "<!doctype html><html><head>",
      "<template><base href=\"https://bad/\"></template>",
      "<base href=\"https://good/\">",
      "</head></html>"
    );
    let (base_url, doc) = parse_with_base_url(html);
    assert_eq!(base_url.as_deref(), Some("https://good/"));

    let base_nodes: Vec<NodeId> = (0..doc.nodes_len())
      .filter_map(|idx| {
        let id = NodeId(idx);
        match &doc.node(id).kind {
          NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("base") => Some(id),
          _ => None,
        }
      })
      .collect();
    assert_eq!(base_nodes.len(), 2);

    let mut saw_template_base = false;
    for base_id in base_nodes {
      let parent = doc.node(base_id).parent.expect("base should have parent");
      let (in_head, in_foreign_namespace, in_template) =
        Dom2TreeSink::compute_insertion_flags(&doc, parent);
      assert!(in_head, "<base> in <head> should be detected as in_head");
      assert!(
        !in_foreign_namespace,
        "<base> in <head> template subtree should not be in a foreign namespace"
      );
      if in_template {
        saw_template_base = true;
      }
    }
    assert!(saw_template_base, "expected one <base> to be inside a template subtree");
  }

  #[test]
  fn base_in_foreign_namespace_is_ignored_and_does_not_freeze() {
    // The `<base>` is inside an SVG `foreignObject` integration point so it's an HTML namespace
    // element with a foreign namespace ancestor. Even though the `<base>` itself is HTML, we must
    // still treat it as "in a foreign namespace" for base-href selection.
    let html = concat!(
      "<!doctype html><html><head></head><body>",
      "<svg><foreignObject><base href=\"https://bad/\"></foreignObject></svg>",
      "</body></html>",
    );

    let sink = Dom2TreeSink::new(Some("https://example.com/dir/page.html"));
    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: false,
        ..Default::default()
      },
      ..Default::default()
    };

    let tb = TreeBuilder::new(sink, opts.tree_builder);
    let mut tokenizer = Tokenizer::new(tb, opts.tokenizer);
    let mut input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(html));

    loop {
      match tokenizer.feed(&mut input) {
        TokenizerResult::Done => break,
        TokenizerResult::Script(_) => panic!("unexpected script pause with scripting disabled"),
      }
    }
    tokenizer.end();

    let base_url = tokenizer.sink.sink.current_base_url();

    // No `<base>` in `<head>` means the base URL should remain the document URL.
    assert_eq!(base_url.as_deref(), Some("https://example.com/dir/page.html"));

    let doc = tokenizer.sink.sink.document.borrow().clone();

    // Ensure the `<base>` inside SVG foreign content is an HTML namespace element, but its context
    // is still flagged as "in a foreign namespace" due to a foreign ancestor.
    let base_id = (0..doc.nodes_len())
      .find_map(|idx| {
        let id = NodeId(idx);
        match &doc.node(id).kind {
          NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("base") => Some(id),
          _ => None,
        }
      })
      .expect("expected a <base> element inside foreignObject");

    let NodeKind::Element { namespace, .. } = &doc.node(base_id).kind else {
      panic!("expected <base> element node kind");
    };
    assert_eq!(
      namespace,
      "",
      "expected integration point <base> to be in the HTML namespace"
    );

    let parent = doc.node(base_id).parent.expect("base should have parent");
    let (in_head, in_foreign_namespace, in_template) =
      Dom2TreeSink::compute_insertion_flags(&doc, parent);
    assert!(!in_head, "base should not be inside <head> in this markup");
    assert!(in_foreign_namespace);
    assert!(!in_template);

    // Regression check: encountering a `<base>` inside foreign content must not freeze base-href
    // selection. A later valid `<base href>` in `<head>` should still apply.
    tokenizer.sink.sink.base_url_tracker.borrow_mut().on_element_inserted(
      "base",
      "",
      &[("href".to_string(), "https://good/".to_string())],
      true,
      false,
      false,
    );
    assert_eq!(
      tokenizer.sink.sink.current_base_url().as_deref(),
      Some("https://good/")
    );
  }

  #[test]
  fn base_href_does_not_apply_to_script_prepared_before_base_element_is_inserted() {
    let sink = Dom2TreeSink::new(Some("https://example.com/dir/page.html"));
    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
      },
      ..Default::default()
    };
    let tb = TreeBuilder::new(sink, opts.tree_builder);
    let mut tokenizer = Tokenizer::new(tb, opts.tokenizer);
    let mut input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(
      "<!doctype html><html><head><script src=\"a.js\"></script><base href=\"https://ex/base/\"></head></html>",
    ));

    let script_handle = loop {
      match tokenizer.feed(&mut input) {
        TokenizerResult::Script(handle) => break handle,
        TokenizerResult::Done => panic!("expected script pause"),
      }
    };

    // At the script boundary, the following `<base>` hasn't been parsed/inserted yet.
    assert_eq!(
      tokenizer.sink.sink.current_base_url().as_deref(),
      Some("https://example.com/dir/page.html")
    );

    // Resume and finish parsing; the base should apply after it is inserted.
    let _ = script_handle;
    loop {
      match tokenizer.feed(&mut input) {
        TokenizerResult::Done => break,
        TokenizerResult::Script(_) => panic!("unexpected extra script pause"),
      }
    }
    tokenizer.end();

    assert_eq!(
      tokenizer.sink.sink.current_base_url().as_deref(),
      Some("https://ex/base/")
    );
  }

}
