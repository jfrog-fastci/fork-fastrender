use crate::css::loader::{resolve_href, resolve_href_with_base};
use crate::dom::{is_valid_shadow_host_name, ShadowRootMode, HTML_NAMESPACE};
use crate::html::base_url_tracker::BaseUrlTracker;

use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode as HtmlQuirksMode, TreeSink};
use markup5ever::interface::tree_builder::ElemName;
use markup5ever::interface::Attribute as HtmlAttribute;
use markup5ever::{LocalName, Namespace, QualName};
use rustc_hash::FxHashSet;
use selectors::context::QuirksMode as SelectorQuirksMode;
use std::borrow::Cow;
use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::rc::Rc;

use super::{live_mutation::utf16_len, Attribute, Document, NodeId, NodeKind, SlotAssignmentMode, NULL_NAMESPACE};

/// Sentinel handle returned by TreeSink hooks for node types that are intentionally ignored during
/// parsing (currently: processing instructions).
///
/// This handle is never inserted into the document tree. TreeSink insertion methods detect it via
/// `id.index() >= doc.nodes_len()` and treat it as a no-op.
const IGNORED_HANDLE: NodeId = NodeId(usize::MAX);

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

/// html5ever [`TreeSink`] that incrementally builds a live [`Document`].
///
/// Notes:
/// - Comments and doctypes are materialized as `dom2` nodes so `innerHTML`/`outerHTML` and
///   `document.doctype` can observe them (matching the web platform).
/// - Processing instructions are ignored (HTML treats them as parse errors and they are not
///   surfaced by our JS bindings today).
/// - HTML namespace elements store `namespace=""` for compatibility with existing selector logic.
/// - The sink performs parse-time `<base href>` tracking via an internal [`BaseUrlTracker`].
pub struct Dom2TreeSink {
  document: RefCell<Document>,
  base_url_tracker: Rc<RefCell<BaseUrlTracker>>,
  pending_stylesheet_links: RefCell<Vec<(NodeId, String)>>,
  /// Insertion points where an ignored node (currently: processing instruction) occurred.
  ///
  /// Even though processing instructions are ignored, they must still act as boundaries for text
  /// node merging so HTML tokenization boundaries still affect the produced DOM text node splits.
  ignored_insertion_points: RefCell<FxHashSet<(NodeId, Option<NodeId>)>>,
  declarative_shadow_templates: RefCell<HashMap<NodeId, NodeId>>,
}

impl Dom2TreeSink {
  pub fn new(document_url: Option<&str>) -> Self {
    Self {
      document: RefCell::new(Document::new(SelectorQuirksMode::NoQuirks)),
      base_url_tracker: Rc::new(RefCell::new(BaseUrlTracker::new(document_url))),
      pending_stylesheet_links: RefCell::new(Vec::new()),
      ignored_insertion_points: RefCell::new(FxHashSet::default()),
      declarative_shadow_templates: RefCell::new(HashMap::new()),
    }
  }

  /// Create a tree sink for HTML fragment parsing.
  ///
  /// Today this is equivalent to `Dom2TreeSink::new`, but we keep it as a distinct constructor so
  /// fragment parsing call sites stay explicit.
  pub(crate) fn new_for_fragment(document_url: Option<&str>) -> Self {
    Self::new(document_url)
  }

  pub fn base_url_tracker_rc(&self) -> Rc<RefCell<BaseUrlTracker>> {
    Rc::clone(&self.base_url_tracker)
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

  pub(crate) fn take_pending_stylesheet_links(&self) -> Vec<(NodeId, String)> {
    std::mem::take(&mut *self.pending_stylesheet_links.borrow_mut())
  }

  fn map_quirks_mode(mode: HtmlQuirksMode) -> SelectorQuirksMode {
    match mode {
      HtmlQuirksMode::Quirks => SelectorQuirksMode::Quirks,
      HtmlQuirksMode::LimitedQuirks => SelectorQuirksMode::LimitedQuirks,
      HtmlQuirksMode::NoQuirks => SelectorQuirksMode::NoQuirks,
    }
  }

  fn normalize_namespace_for_storage(doc: &Document, ns: &str) -> String {
    if doc.is_html_document() && ns == HTML_NAMESPACE {
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
          if tag_name.eq_ignore_ascii_case("head")
            && doc.is_html_case_insensitive_namespace(namespace)
          {
            in_head = true;
          }
          if !doc.is_html_case_insensitive_namespace(namespace) {
            in_foreign_namespace = true;
          }
        }
        NodeKind::Slot { namespace, .. } => {
          if !doc.is_html_case_insensitive_namespace(namespace) {
            in_foreign_namespace = true;
          }
        }
        NodeKind::ShadowRoot { .. } => {
          // DOM's "root" concept stops at a ShadowRoot boundary. For parse-time `<base href>`
          // tracking, `<base>` elements inside a shadow tree must not be treated as being in the
          // document `<head>`.
          //
          // Note: A shadow tree can itself contain a `<head>` element. That `<head>` must *not*
          // count as being inside the document's `<head>` for base-href selection, so clear any
          // previously-detected `in_head` flag before stopping the walk.
          in_head = false;
          break;
        }
        NodeKind::Document { .. }
        | NodeKind::DocumentFragment
        | NodeKind::Doctype { .. }
        | NodeKind::Comment { .. }
        | NodeKind::ProcessingInstruction { .. }
        | NodeKind::Text { .. } => {}
      }
      current = doc.node(id).parent;
    }

    (in_head, in_foreign_namespace, in_template)
  }

  fn element_info<'a>(kind: &'a NodeKind) -> Option<(&'a str, &'a str, &'a [Attribute])> {
    match kind {
      NodeKind::Element {
        tag_name,
        namespace,
        prefix: _,
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
    doc.node_iterator_pre_remove_steps(target);
    let Some(pos) = doc
      .node(old_parent)
      .children
      .iter()
      .position(|&c| c == target)
    else {
      doc.node_mut(target).parent = None;
      return;
    };

    doc.live_mutation.pre_remove(target, old_parent, pos);
    doc.live_range_pre_remove_steps(target, old_parent, pos);
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
      Some(reference) => doc
        .node(parent)
        .children
        .iter()
        .position(|&c| c == reference),
      None => Some(doc.node(parent).children.len()),
    };
    let Some(insertion_idx) = insertion_idx else {
      return false;
    };

    // Avoid inserting the same node twice.
    if doc.node(parent).children.get(insertion_idx).copied() == Some(child) {
      return false;
    }

    doc.live_mutation.pre_insert(parent, insertion_idx, 1);
    doc.live_range_pre_insert_steps(
      parent,
      doc.tree_child_index_from_raw_index_for_range(parent, insertion_idx),
      doc.inserted_tree_children_count_for_range(parent, &[child]),
    );
    doc.node_mut(parent).children.insert(insertion_idx, child);
    doc.node_mut(child).parent = Some(parent);
    true
  }

  fn append_text_at(
    doc: &mut Document,
    parent: NodeId,
    reference: Option<NodeId>,
    text: &str,
    break_text_merge: bool,
  ) {
    if text.is_empty() {
      return;
    }

    if reference.is_none() {
      if !break_text_merge {
        if let Some(last_child) = doc.node(parent).children.last().copied() {
          if matches!(&doc.node(last_child).kind, NodeKind::Text { .. }) {
            let has_live_subscribers = doc.live_mutation.has_subscribers();
            let has_live_ranges = !doc.ranges.is_empty();
            if has_live_subscribers || has_live_ranges {
              let offset = match &doc.node(last_child).kind {
                NodeKind::Text { content } => utf16_len(content),
                _ => 0,
              };
              let inserted_len = utf16_len(text);
              if has_live_subscribers {
                doc.live_mutation.replace_data(
                  last_child,
                  offset,
                  /* removed_len */ 0,
                  /* inserted_len */ inserted_len,
                );
              }
              if has_live_ranges {
                doc.live_range_replace_data_steps(
                  last_child,
                  offset,
                  /* removed_len */ 0,
                  /* inserted_len */ inserted_len,
                );
              }
            }
            if let NodeKind::Text { content } = &mut doc.node_mut(last_child).kind {
              content.push_str(text);
            }
            return;
          }
        }
      }

      let insertion_idx = doc.node(parent).children.len();
      doc.live_mutation.pre_insert(parent, insertion_idx, 1);
      doc.live_range_pre_insert_steps(
        parent,
        doc.tree_child_index_from_raw_index_for_range(parent, insertion_idx),
        /* count */ 1,
      );
      doc.push_node(
        NodeKind::Text {
          content: text.to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );
      return;
    }

    let Some(reference) = reference else {
      return;
    };
    let Some(insert_pos) = doc
      .node(parent)
      .children
      .iter()
      .position(|&c| c == reference)
    else {
      return;
    };
    let prev = if insert_pos == 0 {
      None
    } else {
      doc.node(parent).children.get(insert_pos - 1).copied()
    };

    let can_merge_prev = !break_text_merge
      && prev.is_some_and(|id| matches!(&doc.node(id).kind, NodeKind::Text { .. }));
    let can_merge_next =
      !break_text_merge && matches!(&doc.node(reference).kind, NodeKind::Text { .. });

    if can_merge_prev {
      let Some(prev_id) = prev else {
        debug_assert!(false, "can_merge_prev implies a previous sibling exists");
        return;
      };
      let has_live_subscribers = doc.live_mutation.has_subscribers();
      let has_live_ranges = !doc.ranges.is_empty();
      let (prev_old_len, inserted_len) = if has_live_subscribers || has_live_ranges {
        (
          match &doc.node(prev_id).kind {
            NodeKind::Text { content } => utf16_len(content),
            _ => 0,
          },
          utf16_len(text),
        )
      } else {
        (0, 0)
      };
      if has_live_subscribers {
        doc.live_mutation.replace_data(
          prev_id,
          prev_old_len,
          /* removed_len */ 0,
          inserted_len,
        );
      }
      if has_live_ranges {
        doc.live_range_replace_data_steps(
          prev_id,
          prev_old_len,
          /* removed_len */ 0,
          /* inserted_len */ inserted_len,
        );
      }
      if let NodeKind::Text { content } = &mut doc.node_mut(prev_id).kind {
        content.push_str(text);
      }

      // If the next sibling is also text, merge it too to avoid adjacent text nodes.
      if can_merge_next {
        let next_content = match &doc.node(reference).kind {
          NodeKind::Text { content } => content.clone(),
          _ => String::new(),
        };
        let next_inserted_len =
          (has_live_subscribers || has_live_ranges).then(|| utf16_len(&next_content));
        if let Some(next_inserted_len) = next_inserted_len {
          if has_live_subscribers {
            doc.live_mutation.replace_data(
              prev_id,
              /* offset */ prev_old_len + inserted_len,
              /* removed_len */ 0,
              /* inserted_len */ next_inserted_len,
            );
          }
          if has_live_ranges {
            doc.live_range_replace_data_steps(
              prev_id,
              prev_old_len + inserted_len,
              /* removed_len */ 0,
              /* inserted_len */ next_inserted_len,
            );
          }
        }
        if has_live_ranges {
          doc.live_range_merge_text_steps(reference, prev_id, prev_old_len + inserted_len);
        }
        if let NodeKind::Text { content } = &mut doc.node_mut(prev_id).kind {
          content.push_str(&next_content);
        }
        Self::detach_from_parent(doc, reference);
      }

      return;
    }

    if can_merge_next {
      let has_live_subscribers = doc.live_mutation.has_subscribers();
      let has_live_ranges = !doc.ranges.is_empty();
      if has_live_subscribers || has_live_ranges {
        let inserted_len = utf16_len(text);
        if has_live_subscribers {
          doc.live_mutation.replace_data(
            reference,
            /* offset */ 0,
            /* removed_len */ 0,
            /* inserted_len */ inserted_len,
          );
        }
        if has_live_ranges {
          doc.live_range_replace_data_steps(
            reference,
            /* offset */ 0,
            /* removed_len */ 0,
            /* inserted_len */ inserted_len,
          );
        }
      }
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
    doc.live_mutation.pre_insert(parent, insert_pos, 1);
    doc.live_range_pre_insert_steps(
      parent,
      doc.tree_child_index_from_raw_index_for_range(parent, insert_pos),
      /* count */ 1,
    );
    doc.node_mut(parent).children.insert(insert_pos, text_id);
    doc.node_mut(text_id).parent = Some(parent);
  }

  fn record_ignored_insertion_point(
    &self,
    doc: &Document,
    parent: NodeId,
    reference: Option<NodeId>,
  ) {
    let mut should_record = false;

    match reference {
      None => {
        // Only needed when the ignored node would have prevented merging with the last text child.
        if doc
          .node(parent)
          .children
          .last()
          .copied()
          .is_some_and(|id| matches!(&doc.node(id).kind, NodeKind::Text { .. }))
        {
          should_record = true;
        }
      }
      Some(reference) => {
        // Insertion is before `reference`; prevent merging with adjacent text siblings at this boundary.
        let Some(insert_pos) = doc
          .node(parent)
          .children
          .iter()
          .position(|&c| c == reference)
        else {
          return;
        };

        if insert_pos > 0 {
          let prev = doc.node(parent).children[insert_pos - 1];
          if matches!(&doc.node(prev).kind, NodeKind::Text { .. }) {
            should_record = true;
          }
        }

        if matches!(&doc.node(reference).kind, NodeKind::Text { .. }) {
          should_record = true;
        }
      }
    }

    if should_record {
      self
        .ignored_insertion_points
        .borrow_mut()
        .insert((parent, reference));
    }
  }

  fn take_ignored_insertion_point(&self, parent: NodeId, reference: Option<NodeId>) -> bool {
    self
      .ignored_insertion_points
      .borrow_mut()
      .remove(&(parent, reference))
  }

  fn note_element_inserted(&self, doc: &Document, parent: NodeId, child: NodeId) {
    let Some((tag_name, namespace, attrs)) = Self::element_info(&doc.node(child).kind) else {
      return;
    };

    fn is_ascii_whitespace_html(c: char) -> bool {
      matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
    }

    fn trim_ascii_whitespace(value: &str) -> &str {
      value.trim_matches(is_ascii_whitespace_html)
    }

    fn link_rel_is_stylesheet(rel: &str) -> bool {
      rel
        .split(is_ascii_whitespace_html)
        .filter(|token| !token.is_empty())
        .any(|token| token.eq_ignore_ascii_case("stylesheet"))
    }

    fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
      haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
    }

    let (in_head, in_foreign_namespace, in_template) =
      if tag_name.eq_ignore_ascii_case("base") || tag_name.eq_ignore_ascii_case("link") {
        Self::compute_insertion_flags(doc, parent)
      } else {
        (false, false, false)
      };

    if tag_name.eq_ignore_ascii_case("base") {
      let attrs_for_tracker: Vec<(String, String)> = attrs
        .iter()
        .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
        .collect();
      self.base_url_tracker.borrow_mut().on_element_inserted(
        tag_name,
        namespace,
        attrs_for_tracker.as_slice(),
        in_head,
        in_foreign_namespace,
        in_template,
      );
      return;
    }
    if tag_name.eq_ignore_ascii_case("link") && doc.is_html_case_insensitive_namespace(namespace) {
      if in_template || in_foreign_namespace {
        return;
      }

      let rel_value = attrs
        .iter()
        .find_map(|attr| {
          (attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case("rel"))
            .then_some(attr.value.as_str())
        });
      if !rel_value.is_some_and(link_rel_is_stylesheet) {
        return;
      }

      let Some(href_raw) = attrs
        .iter()
        .find_map(|attr| {
          (attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case("href"))
            .then_some(attr.value.as_str())
        })
      else {
        return;
      };

      let href_trimmed = trim_ascii_whitespace(href_raw);
      if href_trimmed.is_empty() || href_trimmed.starts_with('#') {
        return;
      }

      let base_url = self.base_url_tracker.borrow().current_base_url();
      let resolved = match base_url.as_deref() {
        Some(base) => resolve_href_with_base(Some(base), href_trimmed),
        None => {
          let bytes = href_trimmed.as_bytes();
          if starts_with_ignore_ascii_case(bytes, b"javascript:")
            || starts_with_ignore_ascii_case(bytes, b"vbscript:")
            || starts_with_ignore_ascii_case(bytes, b"mailto:")
          {
            return;
          }
          // Preserve relative URLs (like `a.css`) when the document base is not yet known.
          resolve_href("", href_trimmed).or_else(|| Some(href_trimmed.to_string()))
        }
      };

      if let Some(url) = resolved {
        self
          .pending_stylesheet_links
          .borrow_mut()
          .push((child, url));
      }
    }
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
    // Promote any remaining declarative shadow roots that were parsed as ordinary `<template>`
    // elements (e.g. legacy `shadowroot=` markup, or `<template shadowrootmode=...>` that failed to
    // attach during parsing).
    //
    // Note: `<template shadowrootmode=...>` is primarily handled during parsing via html5ever's
    // declarative shadow DOM hooks (`TreeSink::attach_declarative_shadow`), which ensures shadow
    // roots exist at `<script>` pause points.
    let mut doc = self.document.into_inner();
    doc.attach_shadow_roots();
    doc
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

  fn allow_declarative_shadow_roots(&self, _intended_parent: &NodeId) -> bool {
    let doc = self.document.borrow();
    if _intended_parent.index() >= doc.nodes_len() {
      return false;
    }
    match &doc.node(*_intended_parent).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => doc.is_html_case_insensitive_namespace(namespace) && is_valid_shadow_host_name(tag_name),
      NodeKind::Slot { namespace, .. } => {
        // `attachShadow()` is not permitted on `<slot>`; keep this branch for completeness.
        doc.is_html_case_insensitive_namespace(namespace) && is_valid_shadow_host_name("slot")
      }
      _ => false,
    }
  }

  fn attach_declarative_shadow(
    &self,
    location: &NodeId,
    template: &NodeId,
    attrs: &[HtmlAttribute],
  ) -> bool {
    let mode_attr = attrs.iter().find_map(|attr| {
      attr
        .name
        .local
        .as_ref()
        .eq_ignore_ascii_case("shadowrootmode")
        .then(|| attr.value.to_string())
    });
    let Some(mode_attr) = mode_attr else {
      return false;
    };

    let mode = if mode_attr.eq_ignore_ascii_case("open") {
      ShadowRootMode::Open
    } else if mode_attr.eq_ignore_ascii_case("closed") {
      ShadowRootMode::Closed
    } else {
      return false;
    };
    let delegates_focus = attrs.iter().any(|attr| {
      attr
        .name
        .local
        .as_ref()
        .eq_ignore_ascii_case("shadowrootdelegatesfocus")
    });
    let clonable = attrs.iter().any(|attr| {
      attr
        .name
        .local
        .as_ref()
        .eq_ignore_ascii_case("shadowrootclonable")
    });
    let serializable = attrs.iter().any(|attr| {
      attr
        .name
        .local
        .as_ref()
        .eq_ignore_ascii_case("shadowrootserializable")
    });

    let mut doc = self.document.borrow_mut();
    let is_valid_shadow_host = match &doc.node(*location).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => doc.is_html_case_insensitive_namespace(namespace) && is_valid_shadow_host_name(tag_name),
      NodeKind::Slot { namespace, .. } => {
        // `attachShadow()` is not permitted on `<slot>`; keep this branch for completeness.
        doc.is_html_case_insensitive_namespace(namespace) && is_valid_shadow_host_name("slot")
      }
      _ => false,
    };
    if !is_valid_shadow_host {
      return false;
    }
    if doc
      .node(*location)
      .children
      .iter()
      .any(|&child| matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. }))
    {
      return false;
    }

    let shadow_root_id = doc.push_node(
      NodeKind::ShadowRoot {
        mode,
        delegates_focus,
        slot_assignment: SlotAssignmentMode::Named,
        clonable,
        serializable,
        declarative: true,
      },
      None,
      /* inert_subtree */ false,
    );
    doc.node_mut(shadow_root_id).parent = Some(*location);
    doc.live_mutation.pre_insert(*location, 0, 1);
    doc.live_range_pre_insert_steps(
      *location,
      doc.tree_child_index_from_raw_index_for_range(*location, 0),
      doc.inserted_tree_children_count_for_range(*location, &[shadow_root_id]),
    );
    doc.node_mut(*location).children.insert(0, shadow_root_id);

    self
      .declarative_shadow_templates
      .borrow_mut()
      .insert(*template, shadow_root_id);
    true
  }

  fn elem_name(&self, target: &NodeId) -> Dom2ElemName {
    let doc = self.document.borrow();
    if target.index() >= doc.nodes_len() {
      return Dom2ElemName {
        ns: Namespace::from(HTML_NAMESPACE),
        local: LocalName::from(""),
      };
    }
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

  fn create_element(&self, name: QualName, attrs: Vec<HtmlAttribute>, flags: ElementFlags) -> NodeId {
    let (namespace, is_html_namespace) = {
      let doc = self.document.borrow();
      let namespace = Self::normalize_namespace_for_storage(&doc, name.ns.as_ref());
      let is_html_namespace = doc.is_html_case_insensitive_namespace(namespace.as_str());
      (namespace, is_html_namespace)
    };
    let mut attributes = Vec::with_capacity(attrs.len());
    for attr in attrs {
      // `markup5ever` uses the empty namespace for "no namespace"; `dom2` needs a distinct sentinel
      // because it uses the empty string to represent the HTML namespace.
      let ns_uri = attr.name.ns.as_ref();
      let namespace = if ns_uri.is_empty() {
        NULL_NAMESPACE.to_string()
      } else if ns_uri == HTML_NAMESPACE {
        String::new()
      } else {
        ns_uri.to_string()
      };
      let mut prefix = attr.name.prefix.as_ref().map(|p| p.to_string());
      if namespace == NULL_NAMESPACE {
        prefix = None;
      }
      attributes.push(Attribute {
        namespace,
        prefix,
        local_name: attr.name.local.to_string(),
        value: attr.value.to_string(),
      });
    }

    let is_html_slot = name.local.as_ref().eq_ignore_ascii_case("slot") && is_html_namespace;
    let is_html_script = name.local.as_ref().eq_ignore_ascii_case("script") && is_html_namespace;

    let inert_subtree = name.local.as_ref().eq_ignore_ascii_case("template") && is_html_namespace;
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
        prefix: None,
        attributes,
      }
    };

    let mut doc = self.document.borrow_mut();
    let id = doc.push_node(kind, None, inert_subtree);
    if let Some(node) = doc.nodes.get_mut(id.index()) {
      node.mathml_annotation_xml_integration_point = flags.mathml_annotation_xml_integration_point;
      if is_html_script {
        node.script_force_async = false;
        node.script_parser_document = true;
      }
    }
    id
  }

  fn create_comment(&self, text: StrTendril) -> NodeId {
    let mut doc = self.document.borrow_mut();
    doc.push_node(
      NodeKind::Comment {
        content: text.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
    let _ = (target, data);
    IGNORED_HANDLE
  }

  fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
    match child {
      NodeOrText::AppendText(text) => {
        let mut doc = self.document.borrow_mut();
        let break_text_merge = self.take_ignored_insertion_point(*parent, None);
        Self::append_text_at(&mut doc, *parent, None, text.as_ref(), break_text_merge);
      }
      NodeOrText::AppendNode(node) => {
        let mut doc = self.document.borrow_mut();
        if node.index() >= doc.nodes_len() {
          self.record_ignored_insertion_point(&doc, *parent, None);
          return;
        }
        // Any ignored insertion boundary at this insertion point is now superseded by a real node.
        let _ = self.take_ignored_insertion_point(*parent, None);
        if Self::insert_node_before(&mut doc, *parent, None, node) {
          self.note_element_inserted(&doc, *parent, node);
        }
      }
    }
  }

  fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
    let parent = {
      let doc = self.document.borrow();
      if sibling.index() >= doc.nodes_len() {
        return;
      }
      doc.node(*sibling).parent
    };
    let Some(parent) = parent else {
      return;
    };

    match child {
      NodeOrText::AppendText(text) => {
        let mut doc = self.document.borrow_mut();
        let break_text_merge = self.take_ignored_insertion_point(parent, Some(*sibling));
        Self::append_text_at(
          &mut doc,
          parent,
          Some(*sibling),
          text.as_ref(),
          break_text_merge,
        );
      }
      NodeOrText::AppendNode(node) => {
        let mut doc = self.document.borrow_mut();
        if node.index() >= doc.nodes_len() {
          self.record_ignored_insertion_point(&doc, parent, Some(*sibling));
          return;
        }
        let _ = self.take_ignored_insertion_point(parent, Some(*sibling));
        if Self::insert_node_before(&mut doc, parent, Some(*sibling), node) {
          self.note_element_inserted(&doc, parent, node);
        }
      }
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
      if element.index() >= doc.nodes_len() {
        None
      } else {
        doc.node(*element).parent
      }
    };
    if parent.is_some() {
      self.append_before_sibling(element, child);
      return;
    }
    self.append(prev_element, child)
  }

  fn append_doctype_to_document(
    &self,
    name: StrTendril,
    public_id: StrTendril,
    system_id: StrTendril,
  ) {
    let mut doc = self.document.borrow_mut();
    let root = doc.root();
    // html5ever can synthesize the `<html>` element early during parsing (before an authored
    // `<!doctype>` token is processed). The DOM tree, however, expects the doctype (when present) to
    // precede the document element in tree order.
    //
    // Insert the doctype before the first element/slot child so `document.firstChild` and
    // `document.childNodes` reflect spec ordering.
    let reference = doc
      .node(root)
      .children
      .iter()
      .copied()
      .find(|&child| matches!(&doc.node(child).kind, NodeKind::Element { .. } | NodeKind::Slot { .. }));
    let doctype = doc.push_node(
      NodeKind::Doctype {
        name: name.to_string(),
        public_id: public_id.to_string(),
        system_id: system_id.to_string(),
      },
      None,
      /* inert_subtree */ false,
    );
    Self::insert_node_before(&mut doc, root, reference, doctype);
  }

  fn get_template_contents(&self, target: &NodeId) -> NodeId {
    // FastRender represents template contents as children of the `<template>` element itself.
    self
      .declarative_shadow_templates
      .borrow()
      .get(target)
      .copied()
      .unwrap_or(*target)
  }

  fn remove_from_parent(&self, target: &NodeId) {
    let mut doc = self.document.borrow_mut();
    if target.index() >= doc.nodes_len() {
      return;
    }
    Self::detach_from_parent(&mut doc, *target);
  }

  fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
    let mut doc = self.document.borrow_mut();
    if node.index() >= doc.nodes_len() || new_parent.index() >= doc.nodes_len() {
      return;
    }
    let old_len = doc.node(*new_parent).children.len();
    let moved_children_snapshot = doc.node(*node).children.clone();
    if !moved_children_snapshot.is_empty() {
      // Run pre-remove steps in reverse so offsets are updated as though children were removed from
      // the end towards the start. This matches a sequence of individual removals without requiring
      // us to mutate the children list itself until we `take()` it below.
      //
      // Note: NodeIterator pre-remove steps depend on tree traversal order (e.g. finding the
      // following node after a removed subtree). Detach each removed child's parent pointer as we go
      // so subsequent pre-remove calls observe the tree as if removals happened sequentially.
      for (idx, &child) in moved_children_snapshot.iter().enumerate().rev() {
        doc.node_iterator_pre_remove_steps(child);
        doc.live_mutation.pre_remove(child, *node, idx);
        doc.live_range_pre_remove_steps(child, *node, idx);
        doc.node_mut(child).parent = None;
      }
      doc
        .live_mutation
        .pre_insert(*new_parent, old_len, moved_children_snapshot.len());
      doc.live_range_pre_insert_steps(
        *new_parent,
        doc.tree_child_index_from_raw_index_for_range(*new_parent, old_len),
        doc.inserted_tree_children_count_for_range(*new_parent, &moved_children_snapshot),
      );
    }
    let moved_children = std::mem::take(&mut doc.node_mut(*node).children);
    if moved_children.is_empty() {
      return;
    }
    for &child in &moved_children {
      doc.node_mut(child).parent = None;
    }
    doc
      .node_mut(*new_parent)
      .children
      .extend(moved_children.iter().copied());
    for &child in &moved_children {
      doc.node_mut(child).parent = Some(*new_parent);
    }

    // Merge boundary text nodes if reparenting created a new adjacency.
    if old_len > 0 {
      let prev = doc.node(*new_parent).children[old_len - 1];
      let next = doc.node(*new_parent).children[old_len];
      if matches!(&doc.node(prev).kind, NodeKind::Text { .. })
        && matches!(&doc.node(next).kind, NodeKind::Text { .. })
      {
        let next_content = match &doc.node(next).kind {
          NodeKind::Text { content } => content.clone(),
          _ => String::new(),
        };
        let has_live_subscribers = doc.live_mutation.has_subscribers();
        let has_live_ranges = !doc.ranges.is_empty();
        let (offset, inserted_len) = if has_live_subscribers || has_live_ranges {
          (
            match &doc.node(prev).kind {
              NodeKind::Text { content } => utf16_len(content),
              _ => 0,
            },
            utf16_len(&next_content),
          )
        } else {
          (0, 0)
        };
        if has_live_subscribers {
          doc.live_mutation.replace_data(
            prev,
            offset,
            /* removed_len */ 0,
            /* inserted_len */ inserted_len,
          );
        }
        if has_live_ranges {
          doc.live_range_replace_data_steps(
            prev,
            offset,
            /* removed_len */ 0,
            /* inserted_len */ inserted_len,
          );
          doc.live_range_merge_text_steps(next, prev, offset);
        }
        if let NodeKind::Text { content } = &mut doc.node_mut(prev).kind {
          content.push_str(&next_content);
        } else {
          debug_assert!(false, "prev kind checked above");
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

  fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<HtmlAttribute>) {
    let Some(is_html) = (|| {
      let doc = self.document.borrow();
      if target.index() >= doc.nodes_len() {
        return None;
      }
      match &doc.node(*target).kind {
        NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => {
          Some(doc.is_html_case_insensitive_namespace(namespace))
        }
        _ => None,
      }
    })() else {
      return;
    };

    let mut doc = self.document.borrow_mut();
    if target.index() >= doc.nodes_len() {
      return;
    }
    let kind = &mut doc.node_mut(*target).kind;
    let existing = match kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
      _ => return,
    };

    for attr in attrs {
      let ns_uri = attr.name.ns.as_ref();
      let namespace = if ns_uri.is_empty() {
        NULL_NAMESPACE.to_string()
      } else if ns_uri == HTML_NAMESPACE {
        String::new()
      } else {
        ns_uri.to_string()
      };
      let mut prefix = attr.name.prefix.as_ref().map(|p| p.to_string());
      if namespace == NULL_NAMESPACE {
        prefix = None;
      }

      let local_name = attr.name.local.to_string();
      let ci = is_html && namespace == NULL_NAMESPACE;
      let present = existing.iter().any(|existing_attr| {
        existing_attr.namespace == namespace
          && if ci {
            existing_attr.local_name.eq_ignore_ascii_case(local_name.as_str())
          } else {
            existing_attr.local_name == local_name
          }
      });
      if present {
        continue;
      }
      existing.push(Attribute {
        namespace,
        prefix,
        local_name,
        value: attr.value.to_string(),
      });
    }
  }

  fn mark_script_already_started(&self, node: &NodeId) {
    let mut doc = self.document.borrow_mut();
    if node.index() >= doc.nodes_len() {
      return;
    }
    let _ = doc.set_script_already_started(*node, true);
  }

  fn is_mathml_annotation_xml_integration_point(&self, node: &NodeId) -> bool {
    let doc = self.document.borrow();
    if node.index() >= doc.nodes_len() {
      return false;
    }
    doc.node(*node).mathml_annotation_xml_integration_point
  }

  fn pop(&self, _node: &NodeId) {}
}

#[cfg(test)]
mod tests {
  use super::Dom2TreeSink;
  use crate::debug::snapshot::snapshot_dom;
  use crate::dom::HTML_NAMESPACE;
  use crate::dom2::{Document, NodeId, NodeKind};
  use html5ever::tendril::StrTendril;
  use html5ever::tendril::TendrilSink;
  use html5ever::tree_builder::{ElementFlags, NodeOrText, TreeSink};
  use html5ever::ParseOpts;
  use markup5ever::interface::Attribute;
  use markup5ever::{LocalName, Namespace, QualName};
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
  fn ignores_leading_comments_and_matches_legacy_parse_html() {
    let html = "<!--x--><div id=a></div>";
    let expected = crate::dom::parse_html(html).unwrap();
    let doc = parse_with_sink(html);
    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));
    assert!(
      !doc
        .nodes()
        .iter()
        .any(|node| matches!(node.kind, NodeKind::Comment { .. })),
      "comments should not be materialized in dom2 HTML parsing"
    );
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
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("template") => {
          Some(NodeId(idx))
        }
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
    let html_missing_doctype = "<p>x</p>";
    let html_quirks = "<!doctype html public \"-//W3C//DTD HTML 3.2 Final//EN\"><p>x</p>";

    let expected_no_quirks = crate::dom::parse_html(html_no_quirks).unwrap();
    let expected_missing_doctype = crate::dom::parse_html(html_missing_doctype).unwrap();
    let expected_quirks = crate::dom::parse_html(html_quirks).unwrap();

    let doc_no_quirks = parse_with_sink(html_no_quirks);
    let doc_missing_doctype = parse_with_sink(html_missing_doctype);
    let doc_quirks = parse_with_sink(html_quirks);

    assert_eq!(
      expected_no_quirks.document_quirks_mode(),
      doc_no_quirks.to_renderer_dom().document_quirks_mode()
    );
    assert_eq!(
      expected_missing_doctype.document_quirks_mode(),
      doc_missing_doctype.to_renderer_dom().document_quirks_mode()
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
    assert_eq!(
      doc_missing_doctype
        .node(doc_missing_doctype.root())
        .kind
        .clone(),
      NodeKind::Document {
        quirks_mode: expected_missing_doctype.document_quirks_mode()
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
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("p") => {
          Some(NodeId(idx))
        }
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
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("div") => {
          Some(NodeId(idx))
        }
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
  fn ignored_comment_prevents_text_merge_across_boundary_when_streaming_input() {
    let html = "<!doctype html><div>a<!--c-->b</div>";
    let expected = crate::dom::parse_html(html).unwrap();

    let sink = Dom2TreeSink::new(None);
    let mut parser = html5ever::parse_document(sink, ParseOpts::default());
    parser.process("<!doctype html><div>".into());
    parser.process("a".into());
    parser.process("<!--c-->".into());
    parser.process("b</div>".into());
    let doc = parser.finish();

    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));

    let div_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("div") => {
          Some(NodeId(idx))
        }
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
  fn materializes_doctype_and_comment_nodes_in_dom2() {
    let html = "<!doctype html><!--x--><div id=a><!--c--></div>";
    let expected = crate::dom::parse_html(html).unwrap();

    let doc = parse_with_sink(html);
    let snapshot = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&expected), snapshot_dom(&snapshot));

    assert!(
      doc
        .nodes()
        .iter()
        .any(|node| matches!(node.kind, NodeKind::Doctype { .. })),
      "expected doctype nodes to be materialized in dom2 HTML parsing"
    );
    assert!(
      doc
        .nodes()
        .iter()
        .any(|node| matches!(node.kind, NodeKind::Comment { .. })),
      "expected comment nodes to be materialized in dom2 HTML parsing"
    );
    assert!(
      !doc
        .nodes()
        .iter()
        .any(|node| matches!(node.kind, NodeKind::ProcessingInstruction { .. })),
      "processing instructions should not be materialized in dom2 HTML parsing"
    );
  }

  #[test]
  fn ignores_processing_instructions() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();
    let before_len = sink.document().nodes_len();
    let pi = sink.create_pi("xml".into(), "version=\"1.0\"".into());
    // Creating a PI should not allocate a node.
    assert_eq!(sink.document().nodes_len(), before_len);
    sink.append(&root, NodeOrText::AppendNode(pi));
    // Appending a PI should be a no-op and should not allocate nodes.
    assert_eq!(sink.document().nodes_len(), before_len);
  }

  fn html_name(local: &str) -> QualName {
    QualName::new(
      None,
      Namespace::from(HTML_NAMESPACE),
      LocalName::from(local),
    )
  }

  fn attr(local: &str, value: &str) -> Attribute {
    Attribute {
      name: QualName::new(None, Namespace::from(""), LocalName::from(local)),
      value: StrTendril::from_slice(value),
    }
  }

  #[test]
  fn add_attrs_if_missing_does_not_overwrite_existing_attributes() {
    let sink = Dom2TreeSink::new(None);
    let div = sink.create_element(
      html_name("div"),
      vec![attr("id", "a")],
      ElementFlags::default(),
    );

    sink.add_attrs_if_missing(&div, vec![attr("ID", "b"), attr("class", "c")]);

    let doc = sink.document();
    let NodeKind::Element { attributes, .. } = &doc.node(div).kind else {
      panic!("expected element node");
    };
    assert!(
      attributes
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == "a"),
      "expected existing id attribute to be preserved"
    );
    assert!(
      attributes
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("class") && v == "c"),
      "expected missing class attribute to be added"
    );
    assert_eq!(
      attributes.len(),
      2,
      "expected no duplicate/overwritten attributes"
    );
  }

  #[test]
  fn append_before_sibling_merges_inserted_text_with_adjacent_text_nodes() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();
    let parent = sink.create_element(html_name("p"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(parent));

    sink.append(&parent, NodeOrText::AppendText("a".into()));
    let span = sink.create_element(html_name("span"), Vec::new(), ElementFlags::default());
    sink.append(&parent, NodeOrText::AppendNode(span));
    sink.append_before_sibling(&span, NodeOrText::AppendText("b".into()));

    {
      let doc = sink.document();
      let children = &doc.node(parent).children;
      assert_eq!(children.len(), 2);
      let NodeKind::Text { content } = &doc.node(children[0]).kind else {
        panic!("expected first child to be text");
      };
      assert_eq!(content, "ab");
    }

    let parent2 = sink.create_element(html_name("p"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(parent2));
    let span2 = sink.create_element(html_name("span"), Vec::new(), ElementFlags::default());
    sink.append(&parent2, NodeOrText::AppendNode(span2));
    sink.append(&parent2, NodeOrText::AppendText("b".into()));
    let text_id = {
      let doc = sink.document();
      *doc
        .node(parent2)
        .children
        .last()
        .expect("text child exists")
    };
    sink.append_before_sibling(&text_id, NodeOrText::AppendText("a".into()));

    let doc = sink.document();
    let children = &doc.node(parent2).children;
    assert_eq!(children.len(), 2);
    let NodeKind::Text { content } = &doc.node(children[1]).kind else {
      panic!("expected second child to be text");
    };
    assert_eq!(content, "ab");
  }

  #[test]
  fn append_based_on_parent_node_inserts_before_element_when_connected() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let container = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(container));

    let table = sink.create_element(html_name("table"), Vec::new(), ElementFlags::default());
    sink.append(&container, NodeOrText::AppendNode(table));

    sink.append_based_on_parent_node(&table, &container, NodeOrText::AppendText("x".into()));

    let doc = sink.document();
    let children = &doc.node(container).children;
    assert_eq!(children.len(), 2);
    let NodeKind::Text { content } = &doc.node(children[0]).kind else {
      panic!("expected inserted text node");
    };
    assert_eq!(content, "x");
    assert_eq!(children[1], table);
  }

  #[test]
  fn append_based_on_parent_node_appends_to_prev_element_when_element_detached() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let parent = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(parent));

    let marker = sink.create_element(html_name("span"), Vec::new(), ElementFlags::default());
    sink.append(&parent, NodeOrText::AppendNode(marker));

    let detached = sink.create_element(html_name("table"), Vec::new(), ElementFlags::default());
    sink.append_based_on_parent_node(&detached, &parent, NodeOrText::AppendText("y".into()));

    let doc = sink.document();
    let children = &doc.node(parent).children;
    assert_eq!(children.len(), 2);
    assert_eq!(children[0], marker);
    let NodeKind::Text { content } = &doc.node(children[1]).kind else {
      panic!("expected appended text node");
    };
    assert_eq!(content, "y");
  }

  #[test]
  fn reparent_children_updates_parents_and_merges_boundary_text_nodes() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let from = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    let to = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(from));
    sink.append(&root, NodeOrText::AppendNode(to));

    sink.append(&to, NodeOrText::AppendText("hello".into()));
    sink.append(&from, NodeOrText::AppendText("world".into()));

    let moved_text_id = {
      let doc = sink.document();
      doc.node(from).children[0]
    };

    sink.reparent_children(&from, &to);

    let doc = sink.document();
    assert!(doc.node(from).children.is_empty());
    assert_eq!(doc.node(to).children.len(), 1);
    let NodeKind::Text { content } = &doc.node(doc.node(to).children[0]).kind else {
      panic!("expected merged text node");
    };
    assert_eq!(content, "helloworld");
    assert_eq!(
      doc.node(moved_text_id).parent,
      None,
      "merged-away text node should be detached"
    );
  }

  #[test]
  fn reparent_children_updates_live_ranges_for_removed_children() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let from = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    let to = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(from));
    sink.append(&root, NodeOrText::AppendNode(to));

    sink.append(&from, NodeOrText::AppendText("world".into()));

    let range = {
      let mut doc = sink.document_mut();
      let range = doc.create_range();
      // Boundary point after the single text child.
      doc.range_set_start(range, from, 1).unwrap();
      doc.range_set_end(range, from, 1).unwrap();
      range
    };

    sink.reparent_children(&from, &to);

    let doc = sink.document();
    assert!(doc.node(from).children.is_empty());
    assert_eq!(doc.range_start_container(range).unwrap(), from);
    assert_eq!(doc.range_end_container(range).unwrap(), from);
    assert_eq!(doc.range_start_offset(range).unwrap(), 0);
    assert_eq!(doc.range_end_offset(range).unwrap(), 0);
  }

  #[test]
  fn reparent_children_updates_node_iterators_for_removed_children() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let from = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    let to = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(from));
    sink.append(&root, NodeOrText::AppendNode(to));

    let a = sink.create_element(html_name("a"), Vec::new(), ElementFlags::default());
    let b = sink.create_element(html_name("b"), Vec::new(), ElementFlags::default());
    sink.append(&from, NodeOrText::AppendNode(a));
    sink.append(&from, NodeOrText::AppendNode(b));

    let iter = {
      let mut doc = sink.document_mut();
      let iter = doc.create_node_iterator(from);
      doc.set_node_iterator_reference_and_pointer(iter, b, /* pointer_before_reference */ false);
      iter
    };

    sink.reparent_children(&from, &to);

    let doc = sink.document();
    assert!(doc.node(from).children.is_empty());
    assert_eq!(
      doc.node_iterator_reference(iter),
      Some(from),
      "removing all children from a NodeIterator root should update the iterator reference to the root"
    );
    assert_eq!(doc.node_iterator_pointer_before_reference(iter), Some(false));
  }

  #[test]
  fn reparent_children_node_iterator_pointer_before_reference_true_does_not_reference_moved_node() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let from = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    let to = sink.create_element(html_name("div"), Vec::new(), ElementFlags::default());
    sink.append(&root, NodeOrText::AppendNode(from));
    sink.append(&root, NodeOrText::AppendNode(to));

    let a = sink.create_element(html_name("a"), Vec::new(), ElementFlags::default());
    let b = sink.create_element(html_name("b"), Vec::new(), ElementFlags::default());
    sink.append(&from, NodeOrText::AppendNode(a));
    sink.append(&from, NodeOrText::AppendNode(b));

    // Point the iterator at `a` with the pointer before the reference. When `a` is removed, the
    // NodeIterator pre-remove algorithm would normally try to advance to the following node. Since
    // `b` is also being removed, we must ensure `b` is treated as already removed when processing
    // `a` (otherwise the iterator could end up referencing a moved node).
    let iter = {
      let mut doc = sink.document_mut();
      let iter = doc.create_node_iterator(from);
      doc.set_node_iterator_reference_and_pointer(iter, a, /* pointer_before_reference */ true);
      iter
    };

    sink.reparent_children(&from, &to);

    let doc = sink.document();
    assert!(doc.node(from).children.is_empty());
    assert_eq!(doc.node_iterator_reference(iter), Some(from));
    assert_eq!(doc.node_iterator_pointer_before_reference(iter), Some(false));
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
    assert_eq!(
      parse_and_capture_base_url(html).as_deref(),
      Some("https://a/")
    );
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
    assert!(
      saw_template_base,
      "expected one <base> to be inside a template subtree"
    );
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
    assert_eq!(
      base_url.as_deref(),
      Some("https://example.com/dir/page.html")
    );

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
      namespace, "",
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
    tokenizer
      .sink
      .sink
      .base_url_tracker
      .borrow_mut()
      .on_element_inserted(
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

#[cfg(test)]
mod live_mutation_hook_tests {
  use super::Dom2TreeSink;
  use crate::dom2::live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
  use crate::dom2::NodeKind;

  use html5ever::tendril::StrTendril;
  use html5ever::tokenizer::{BufferQueue, Tokenizer};
  use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts, TreeSink};
  use html5ever::{ParseOpts, TokenizerResult};

  fn new_element(tag: &str) -> NodeKind {
    NodeKind::Element {
      tag_name: tag.to_string(),
      namespace: String::new(),
      prefix: None,
      attributes: Vec::new(),
    }
  }

  fn install_recorder(sink: &Dom2TreeSink) -> LiveMutationTestRecorder {
    let recorder = LiveMutationTestRecorder::default();
    sink
      .document_mut()
      .live_mutation
      .set_hook(Some(Box::new(recorder.clone())));
    recorder
  }

  #[test]
  fn remove_from_parent_emits_pre_remove() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let (parent, child) = {
      let mut doc = sink.document_mut();
      let parent = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let child = doc.push_node(
        new_element("span"),
        Some(parent),
        /* inert_subtree */ false,
      );
      (parent, child)
    };

    let recorder = install_recorder(&sink);
    sink.remove_from_parent(&child);

    let events = recorder.take();
    assert_eq!(
      events,
      vec![LiveMutationEvent::PreRemove {
        node: child,
        old_parent: parent,
        old_index: 0
      }]
    );
  }

  #[test]
  fn insert_node_before_emits_pre_insert() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let (parent, child) = {
      let mut doc = sink.document_mut();
      let parent = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let child = doc.push_node(new_element("span"), None, /* inert_subtree */ false);
      (parent, child)
    };

    let recorder = install_recorder(&sink);

    let inserted = {
      let mut doc = sink.document_mut();
      Dom2TreeSink::insert_node_before(&mut doc, parent, None, child)
    };
    assert!(inserted);

    let events = recorder.take();
    assert_eq!(
      events,
      vec![LiveMutationEvent::PreInsert {
        parent,
        index: 0,
        count: 1
      }]
    );
  }

  #[test]
  fn append_text_at_merges_prev_and_next_and_emits_replace_data_then_pre_remove() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let (parent, prev_text, next_text) = {
      let mut doc = sink.document_mut();
      let parent = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let prev_text = doc.push_node(
        NodeKind::Text {
          content: "a".to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );
      let next_text = doc.push_node(
        NodeKind::Text {
          content: "b".to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );
      (parent, prev_text, next_text)
    };

    let recorder = install_recorder(&sink);

    {
      let mut doc = sink.document_mut();
      Dom2TreeSink::append_text_at(&mut doc, parent, Some(next_text), "x", false);
    }

    let events = recorder.take();
    assert_eq!(
      events,
      vec![
        LiveMutationEvent::ReplaceData {
          node: prev_text,
          offset: 1,
          removed_len: 0,
          inserted_len: 1
        },
        LiveMutationEvent::ReplaceData {
          node: prev_text,
          offset: 2,
          removed_len: 0,
          inserted_len: 1
        },
        LiveMutationEvent::PreRemove {
          node: next_text,
          old_parent: parent,
          old_index: 1
        }
      ]
    );
  }

  #[test]
  fn append_text_at_merges_next_text_node_and_moves_live_ranges_into_prev() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let (parent, prev_text, next_text) = {
      let mut doc = sink.document_mut();
      let parent = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let prev_text = doc.push_node(
        NodeKind::Text {
          content: "a".to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );
      let next_text = doc.push_node(
        NodeKind::Text {
          content: "b".to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );
      (parent, prev_text, next_text)
    };

    let range = {
      let mut doc = sink.document_mut();
      let range = doc.create_range();
      // Boundary point at the end of the second text node ("b").
      doc.range_set_start(range, next_text, 1).unwrap();
      doc.range_set_end(range, next_text, 1).unwrap();
      range
    };

    {
      let mut doc = sink.document_mut();
      Dom2TreeSink::append_text_at(&mut doc, parent, Some(next_text), "x", false);
    }

    let doc = sink.document();
    // "a" + inserted "x" + merged-away "b"
    let NodeKind::Text { content } = &doc.node(prev_text).kind else {
      panic!("expected merged text node");
    };
    assert_eq!(content, "axb");

    // The range should be moved into the surviving text node with an offset shift of 2 ("ax").
    assert_eq!(doc.range_start_container(range).unwrap(), prev_text);
    assert_eq!(doc.range_end_container(range).unwrap(), prev_text);
    assert_eq!(doc.range_start_offset(range).unwrap(), 3);
    assert_eq!(doc.range_end_offset(range).unwrap(), 3);
  }

  #[test]
  fn reparent_children_emits_bulk_hooks_and_boundary_merge_events() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let (from, to, to_text, moved_text) = {
      let mut doc = sink.document_mut();
      let from = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let to = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let to_text = doc.push_node(
        NodeKind::Text {
          content: "hello".to_string(),
        },
        Some(to),
        /* inert_subtree */ false,
      );
      let moved_text = doc.push_node(
        NodeKind::Text {
          content: "world".to_string(),
        },
        Some(from),
        /* inert_subtree */ false,
      );
      (from, to, to_text, moved_text)
    };

    let recorder = install_recorder(&sink);
    sink.reparent_children(&from, &to);

    let doc = sink.document();
    assert!(doc.node(from).children.is_empty());
    assert_eq!(doc.node(to).children.len(), 1);
    let NodeKind::Text { content } = &doc.node(doc.node(to).children[0]).kind else {
      panic!("expected merged text node");
    };
    assert_eq!(content, "helloworld");

    let events = recorder.take();
    assert_eq!(
      events,
      vec![
        LiveMutationEvent::PreRemove {
          node: moved_text,
          old_parent: from,
          old_index: 0
        },
        LiveMutationEvent::PreInsert {
          parent: to,
          index: 1,
          count: 1
        },
        LiveMutationEvent::ReplaceData {
          node: to_text,
          offset: 5,
          removed_len: 0,
          inserted_len: 5
        },
        LiveMutationEvent::PreRemove {
          node: moved_text,
          old_parent: to,
          old_index: 1
        }
      ]
    );
  }

  #[test]
  fn streaming_parse_after_script_pause_emits_live_hooks() {
    let sink = Dom2TreeSink::new(None);
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
      "<!doctype html><html><body><p>before</p><script src=\"a.js\"></script><p id=after>after</p></body></html>",
    ));

    let script_handle = loop {
      match tokenizer.feed(&mut input) {
        TokenizerResult::Script(handle) => break handle,
        TokenizerResult::Done => panic!("expected script pause"),
      }
    };

    // Simulate script execution creating live objects: install the hook recorder while parsing is
    // paused, then resume parsing and assert subsequent parser insertions emit hook events.
    let recorder = {
      let recorder = LiveMutationTestRecorder::default();
      tokenizer
        .sink
        .sink
        .document
        .borrow_mut()
        .live_mutation
        .set_hook(Some(Box::new(recorder.clone())));
      recorder
    };

    let _ = script_handle;
    loop {
      match tokenizer.feed(&mut input) {
        TokenizerResult::Done => break,
        TokenizerResult::Script(_) => panic!("unexpected extra script pause"),
      }
    }
    tokenizer.end();

    let doc = tokenizer.sink.sink.document.borrow().clone();
    let body = doc.body().expect("expected <body>");
    let after = doc
      .get_element_by_id("after")
      .expect("expected <p id=after>");
    let after_idx = doc
      .index_of_child(body, after)
      .expect("index_of_child")
      .expect("expected after element to be a body child");

    let events = recorder.take();
    assert!(
      events.iter().any(|event| matches!(
        event,
        LiveMutationEvent::PreInsert { parent, index, count }
          if *parent == body && *index == after_idx && *count == 1
      )),
      "expected pre_insert hook for <p id=after> insertion into <body>, got {events:?}"
    );
  }
}

#[cfg(test)]
mod live_range_tests {
  use super::Dom2TreeSink;
  use crate::dom2::NodeKind;

  fn new_element(tag: &str) -> NodeKind {
    NodeKind::Element {
      tag_name: tag.to_string(),
      namespace: String::new(),
      prefix: None,
      attributes: Vec::new(),
    }
  }

  #[test]
  fn insert_node_before_updates_live_ranges() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let (range, parent) = {
      let mut doc = sink.document_mut();
      let parent = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let _child1 = doc.push_node(
        new_element("span"),
        Some(parent),
        /* inert_subtree */ false,
      );
      let child2 = doc.push_node(
        new_element("span"),
        Some(parent),
        /* inert_subtree */ false,
      );
      let inserted = doc.push_node(new_element("p"), None, /* inert_subtree */ false);

      let range = doc.create_range();
      doc.range_set_start(range, parent, 2).unwrap();
      doc.range_set_end(range, parent, 2).unwrap();

      assert!(Dom2TreeSink::insert_node_before(
        &mut doc,
        parent,
        Some(child2),
        inserted
      ));

      (range, parent)
    };

    let doc = sink.document();
    assert_eq!(doc.range_start_container(range).unwrap(), parent);
    assert_eq!(doc.range_start_offset(range).unwrap(), 3);
    assert_eq!(doc.range_end_container(range).unwrap(), parent);
    assert_eq!(doc.range_end_offset(range).unwrap(), 3);
  }

  #[test]
  fn append_text_at_merge_updates_live_ranges() {
    let sink = Dom2TreeSink::new(None);
    let root = sink.get_document();

    let (range, text_id) = {
      let mut doc = sink.document_mut();
      let parent = doc.push_node(
        new_element("div"),
        Some(root),
        /* inert_subtree */ false,
      );
      let text_id = doc.push_node(
        NodeKind::Text {
          content: "b".to_string(),
        },
        Some(parent),
        /* inert_subtree */ false,
      );

      let range = doc.create_range();
      doc.range_set_start(range, text_id, 1).unwrap();
      doc.range_set_end(range, text_id, 1).unwrap();

      Dom2TreeSink::append_text_at(&mut doc, parent, Some(text_id), "a", false);

      (range, text_id)
    };

    let doc = sink.document();
    let NodeKind::Text { content } = &doc.node(text_id).kind else {
      panic!("expected text node");
    };
    assert_eq!(content, "ab");
    assert_eq!(doc.range_start_container(range).unwrap(), text_id);
    assert_eq!(doc.range_start_offset(range).unwrap(), 2);
    assert_eq!(doc.range_end_container(range).unwrap(), text_id);
    assert_eq!(doc.range_end_offset(range).unwrap(), 2);
  }
}
