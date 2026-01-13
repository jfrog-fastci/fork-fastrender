//! Minimal DOM (WHATWG DOM-ish) helpers used by the JavaScript binding layer.
//!
//! These bindings are intentionally scoped: they provide enough DOM mutation surface to exercise
//! the renderer invalidation plumbing in `BrowserDocumentDom2`/`BrowserDocument2`.
//!
//! The key design point is that DOM bindings should **not** mutate `dom2::Document` directly. All
//! mutations must go through [`crate::js::DomHost`] so the host can coalesce invalidation and avoid
//! re-rendering when an operation is a no-op.

use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Attribute, DomError, NodeId, NodeKind, NULL_NAMESPACE};
use crate::js::DomHost;
use crate::web::dom::DomException;

/// `Document.documentElement` (returns the `<html>` element for HTML documents).
pub fn document_element<Host: DomHost + ?Sized>(host: &Host) -> Option<NodeId> {
  host.with_dom(|dom| dom.document_element())
}

/// `Document.head` (returns the first `<head>` element under the document element, if any).
pub fn head<Host: DomHost + ?Sized>(host: &Host) -> Option<NodeId> {
  host.with_dom(|dom| dom.head())
}

/// `Document.body` (returns the first `<body>` element under the document element, if any).
///
/// For HTML documents this may return a `<frameset>` element when no `<body>` is present, mirroring
/// [`crate::dom2::Document::body`].
pub fn body<Host: DomHost + ?Sized>(host: &Host) -> Option<NodeId> {
  host.with_dom(|dom| dom.body())
}

/// `Document.getElementById(id)`.
pub fn get_element_by_id<Host: DomHost + ?Sized>(host: &Host, id: &str) -> Option<NodeId> {
  host.with_dom(|dom| dom.get_element_by_id(id))
}

/// `Document.getElementsByTagName(qualifiedName)` / `Element.getElementsByTagName(qualifiedName)`.
pub fn get_elements_by_tag_name<Host: DomHost + ?Sized>(
  host: &Host,
  root: NodeId,
  qualified_name: &str,
) -> Vec<NodeId> {
  host.with_dom(|dom| dom.get_elements_by_tag_name_from(root, qualified_name))
}

/// `Document.getElementsByClassName(classNames)` / `Element.getElementsByClassName(classNames)`.
pub fn get_elements_by_class_name<Host: DomHost + ?Sized>(
  host: &Host,
  root: NodeId,
  class_names: &str,
) -> Vec<NodeId> {
  host.with_dom(|dom| dom.get_elements_by_class_name_from(root, class_names))
}

// -----------------------------------------------------------------------------------------------
// Read-only node traversal/metadata.

/// `Node.parentNode`.
pub fn parent_node<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.parent_node(node))
}

/// `Node.parentElement`.
pub fn parent_element<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| {
    let parent = dom.parent_node(node)?;
    matches!(
      &dom.nodes().get(parent.index())?.kind,
      NodeKind::Element { .. } | NodeKind::Slot { .. }
    )
    .then_some(parent)
  })
}

/// `Node.firstChild`.
pub fn first_child<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.first_child(node))
}

/// `Node.firstElementChild` / `ParentNode.firstElementChild`.
pub fn first_element_child<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.first_element_child(node))
}

/// `Node.lastChild`.
pub fn last_child<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.last_child(node))
}

/// `Node.lastElementChild` / `ParentNode.lastElementChild`.
pub fn last_element_child<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.last_element_child(node))
}

/// `Node.previousSibling`.
pub fn previous_sibling<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.previous_sibling(node))
}

/// `NonDocumentTypeChildNode.previousElementSibling` / `Element.previousElementSibling`.
pub fn previous_element_sibling<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.previous_element_sibling(node))
}

/// `Node.nextSibling`.
pub fn next_sibling<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.next_sibling(node))
}

/// `NonDocumentTypeChildNode.nextElementSibling` / `Element.nextElementSibling`.
pub fn next_element_sibling<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.next_element_sibling(node))
}

/// `ParentNode.childElementCount`.
pub fn child_element_count<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> usize {
  host.with_dom(|dom| dom.child_element_count(node))
}

/// Snapshot `ParentNode.children` list as a `Vec<NodeId>`.
pub fn children_elements<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Vec<NodeId> {
  host.with_dom(|dom| dom.children_elements(node))
}

/// `Node.isConnected`.
///
/// In `dom2`, this uses `Document::is_connected_for_scripting`, which treats inert `<template>`
/// contents as disconnected (matching the web platform's `template.content` behaviour).
pub fn is_connected<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> bool {
  host.with_dom(|dom| dom.is_connected_for_scripting(node))
}

/// `Node.nodeType`.
///
/// Returns `0` when `node` is not a valid `NodeId` for this document (should be unreachable for
/// correctly-constructed bindings).
pub fn node_type<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> u16 {
  host.with_dom(|dom| {
    let Some(node) = dom.nodes().get(node.index()) else {
      return 0;
    };
    match &node.kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => 1,
      NodeKind::Text { .. } => 3,
      NodeKind::ProcessingInstruction { .. } => 7,
      NodeKind::Comment { .. } => 8,
      NodeKind::Document { .. } => 9,
      NodeKind::Doctype { .. } => 10,
      NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => 11,
    }
  })
}

/// `Node.nodeName`.
///
/// Returns the empty string when `node` is not a valid `NodeId` for this document (should be
/// unreachable for correctly-constructed bindings).
pub fn node_name<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> String {
  host.with_dom(|dom| {
    let Some(node) = dom.nodes().get(node.index()) else {
      return String::new();
    };
    match &node.kind {
      NodeKind::Element { tag_name, .. } => tag_name.to_ascii_uppercase(),
      NodeKind::Slot { .. } => "SLOT".to_string(),
      NodeKind::Text { .. } => "#text".to_string(),
      NodeKind::ProcessingInstruction { target, .. } => target.to_string(),
      NodeKind::Comment { .. } => "#comment".to_string(),
      NodeKind::Document { .. } => "#document".to_string(),
      NodeKind::Doctype { name, .. } => name.to_string(),
      NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
    }
  })
}

/// `Element.tagName`.
///
/// Returns the empty string when `element` is not an element-like node id for this document.
pub fn tag_name<Host: DomHost + ?Sized>(host: &Host, element: NodeId) -> String {
  host.with_dom(|dom| {
    let Some(node) = dom.nodes().get(element.index()) else {
      return String::new();
    };
    match &node.kind {
      NodeKind::Element { tag_name, .. } => tag_name.to_ascii_uppercase(),
      NodeKind::Slot { .. } => "SLOT".to_string(),
      _ => String::new(),
    }
  })
}

// -----------------------------------------------------------------------------------------------
// Node.isEqualNode / concept-node-equals.
//
// Implements WHATWG DOM's `concept-node-equals` for the subset of node kinds supported by `dom2`.
// This is shared by both the handwritten and WebIDL-backed `vm-js` DOM bindings.
//
// Note: `dom2` stores a `ShadowRoot` as a child of its host element so renderer code can traverse
// the composed tree. `Node.isEqualNode()` must compare light-DOM children, so shadow roots are
// filtered out when comparing children of non-shadow-root nodes.
pub(crate) fn is_equal_node_from_dom(
  dom_a: &crate::dom2::Document,
  a_id: NodeId,
  dom_b: &crate::dom2::Document,
  b_id: NodeId,
) -> bool {
  fn normalize_namespace(ns: &str) -> Option<&str> {
    if ns == NULL_NAMESPACE {
      return None;
    }
    if ns.is_empty() || ns == HTML_NAMESPACE {
      return Some(HTML_NAMESPACE);
    }
    Some(ns)
  }

  fn element_like<'a>(
    kind: &'a NodeKind,
  ) -> Option<(&'a str, Option<&'a str>, &'a str, &'a [Attribute])> {
    match kind {
      NodeKind::Element {
        tag_name,
        namespace,
        prefix,
        attributes,
      } => Some((
        namespace.as_str(),
        prefix.as_deref(),
        tag_name.as_str(),
        attributes.as_slice(),
      )),
      NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => Some((namespace.as_str(), None, "slot", attributes.as_slice())),
      _ => None,
    }
  }

  fn attrs_equal(a: &[Attribute], b: &[Attribute]) -> bool {
    if a.len() != b.len() {
      return false;
    }
    let mut matched = vec![false; b.len()];
    'outer: for attr_a in a.iter() {
      let ns_a = normalize_namespace(&attr_a.namespace);
      for (idx, attr_b) in b.iter().enumerate() {
        if matched[idx] {
          continue;
        }
        if ns_a == normalize_namespace(&attr_b.namespace)
          && attr_a.local_name == attr_b.local_name
          && attr_a.value == attr_b.value
        {
          matched[idx] = true;
          continue 'outer;
        }
      }
      return false;
    }
    true
  }

  fn light_tree_children(dom: &crate::dom2::Document, node_id: NodeId) -> Vec<NodeId> {
    let Some(node) = dom.nodes().get(node_id.index()) else {
      return Vec::new();
    };
    let filter_shadow_roots = !matches!(&node.kind, NodeKind::ShadowRoot { .. });

    let mut children: Vec<NodeId> = Vec::with_capacity(node.children.len());
    for &child in node.children.iter() {
      if child.index() >= dom.nodes_len() {
        continue;
      }
      let Some(child_node) = dom.nodes().get(child.index()) else {
        continue;
      };
      if child_node.parent != Some(node_id) {
        continue;
      }
      if filter_shadow_roots && matches!(&child_node.kind, NodeKind::ShadowRoot { .. }) {
        continue;
      }
      children.push(child);
    }
    children
  }

  if a_id.index() >= dom_a.nodes_len() || b_id.index() >= dom_b.nodes_len() {
    return false;
  }

  let mut stack: Vec<(NodeId, NodeId)> = vec![(a_id, b_id)];
  while let Some((a_id, b_id)) = stack.pop() {
    if a_id.index() >= dom_a.nodes_len() || b_id.index() >= dom_b.nodes_len() {
      return false;
    }
    let a_node = &dom_a.nodes()[a_id.index()];
    let b_node = &dom_b.nodes()[b_id.index()];

    match (&a_node.kind, &b_node.kind) {
      (NodeKind::Document { .. }, NodeKind::Document { .. }) => {}
      (NodeKind::DocumentFragment, NodeKind::DocumentFragment) => {}
      (NodeKind::ShadowRoot { .. }, NodeKind::ShadowRoot { .. }) => {}
      (
        NodeKind::Doctype {
          name: a_name,
          public_id: a_public_id,
          system_id: a_system_id,
        },
        NodeKind::Doctype {
          name: b_name,
          public_id: b_public_id,
          system_id: b_system_id,
        },
      ) => {
        if a_name != b_name || a_public_id != b_public_id || a_system_id != b_system_id {
          return false;
        }
      }
      (NodeKind::Text { content: a_text }, NodeKind::Text { content: b_text }) => {
        if a_text != b_text {
          return false;
        }
      }
      (NodeKind::Comment { content: a_text }, NodeKind::Comment { content: b_text }) => {
        if a_text != b_text {
          return false;
        }
      }
      (
        NodeKind::ProcessingInstruction {
          target: a_target,
          data: a_data,
        },
        NodeKind::ProcessingInstruction {
          target: b_target,
          data: b_data,
        },
      ) => {
        if a_target != b_target || a_data != b_data {
          return false;
        }
      }
      (a_kind, b_kind) => {
        let Some((a_ns, a_prefix, a_local, a_attrs)) = element_like(a_kind) else {
          return false;
        };
        let Some((b_ns, b_prefix, b_local, b_attrs)) = element_like(b_kind) else {
          return false;
        };
        if normalize_namespace(a_ns) != normalize_namespace(b_ns) {
          return false;
        }
        if a_prefix != b_prefix {
          return false;
        }
        if a_local != b_local {
          return false;
        }
        if !attrs_equal(a_attrs, b_attrs) {
          return false;
        }
      }
    }

    let a_children = light_tree_children(dom_a, a_id);
    let b_children = light_tree_children(dom_b, b_id);
    if a_children.len() != b_children.len() {
      return false;
    }
    for idx in (0..a_children.len()).rev() {
      stack.push((a_children[idx], b_children[idx]));
    }
  }

  true
}

/// `ParentNode.querySelector(selectors)` for a `dom2` document.
///
/// This uses `dom2`'s selector matching engine, including inert `<template>` behaviour.
///
/// Note: `dom2::Document::query_selector` requires `&mut self` (it snapshots into renderer DOM
/// structures for selector matching), so this is routed through [`DomHost::mutate_dom`] but always
/// reports `changed=false`.
pub fn query_selector<Host: DomHost + ?Sized>(
  host: &mut Host,
  selectors: &str,
  scope: Option<NodeId>,
) -> std::result::Result<Option<NodeId>, DomException> {
  host.mutate_dom(|dom| (dom.query_selector(selectors, scope), false))
}

/// `ParentNode.querySelectorAll(selectors)` for a `dom2` document.
///
/// See [`query_selector`] for notes on DOM mutation tracking.
pub fn query_selector_all<Host: DomHost + ?Sized>(
  host: &mut Host,
  selectors: &str,
  scope: Option<NodeId>,
) -> std::result::Result<Vec<NodeId>, DomException> {
  host.mutate_dom(|dom| (dom.query_selector_all(selectors, scope), false))
}

/// `Element.matches(selectors)` for a `dom2` element.
///
/// Note: `dom2::Document::matches_selector` requires `&mut self` (it snapshots into renderer DOM
/// structures for selector matching), so this is routed through [`DomHost::mutate_dom`] but always
/// reports `changed=false`.
pub fn matches_selector<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  selectors: &str,
) -> std::result::Result<bool, DomException> {
  host.mutate_dom(|dom| (dom.matches_selector(element, selectors), false))
}

/// `Element.closest(selectors)` for a `dom2` element.
///
/// See [`matches_selector`] for notes on DOM mutation tracking.
pub fn closest<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  selectors: &str,
) -> std::result::Result<Option<NodeId>, DomException> {
  host.mutate_dom(|dom| (dom.closest(element, selectors), false))
}

// -----------------------------------------------------------------------------------------------
// Mutation helpers.

/// `Node.appendChild(child)`.
///
/// Returns the `child` node (including when `child` is a `DocumentFragment`, matching WHATWG DOM).
pub fn append_child<Host: DomHost + ?Sized>(
  host: &mut Host,
  parent: NodeId,
  child: NodeId,
) -> std::result::Result<NodeId, DomError> {
  host.mutate_dom(|dom| match dom.append_child(parent, child) {
    Ok(changed) => (Ok(child), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Node.insertBefore(newChild, referenceChild)`.
///
/// Returns the inserted node (`new_child`), including when `new_child` is a `DocumentFragment`.
pub fn insert_before<Host: DomHost + ?Sized>(
  host: &mut Host,
  parent: NodeId,
  new_child: NodeId,
  reference: Option<NodeId>,
) -> std::result::Result<NodeId, DomError> {
  host.mutate_dom(|dom| match dom.insert_before(parent, new_child, reference) {
    Ok(changed) => (Ok(new_child), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Node.removeChild(child)`.
///
/// Returns the removed `child` node.
pub fn remove_child<Host: DomHost + ?Sized>(
  host: &mut Host,
  parent: NodeId,
  child: NodeId,
) -> std::result::Result<NodeId, DomError> {
  host.mutate_dom(|dom| match dom.remove_child(parent, child) {
    Ok(changed) => (Ok(child), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Node.replaceChild(newChild, oldChild)`.
///
/// Returns `old_child`, matching WHATWG DOM.
pub fn replace_child<Host: DomHost + ?Sized>(
  host: &mut Host,
  parent: NodeId,
  new_child: NodeId,
  old_child: NodeId,
) -> std::result::Result<NodeId, DomError> {
  host.mutate_dom(|dom| match dom.replace_child(parent, new_child, old_child) {
    Ok(changed) => (Ok(old_child), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Document.createElement(tagName[, namespace])`.
///
/// This always reports `changed=false`: creating detached nodes must not invalidate the renderer.
pub fn create_element<Host: DomHost + ?Sized>(host: &mut Host, tag_name: &str, namespace: &str) -> NodeId {
  host.mutate_dom(|dom| (dom.create_element(tag_name, namespace), false))
}

/// `Document.createTextNode(data)`.
///
/// This always reports `changed=false`: creating detached nodes must not invalidate the renderer.
pub fn create_text_node<Host: DomHost + ?Sized>(host: &mut Host, data: &str) -> NodeId {
  host.mutate_dom(|dom| (dom.create_text(data), false))
}

/// `Document.createDocumentFragment()`.
///
/// This always reports `changed=false`: creating detached nodes must not invalidate the renderer.
pub fn create_document_fragment<Host: DomHost + ?Sized>(host: &mut Host) -> NodeId {
  host.mutate_dom(|dom| (dom.create_document_fragment(), false))
}

/// `Node.cloneNode(deep)`.
///
/// This always reports `changed=false`: cloned nodes are always detached.
pub fn clone_node<Host: DomHost + ?Sized>(
  host: &mut Host,
  node: NodeId,
  deep: bool,
) -> std::result::Result<NodeId, DomError> {
  host.mutate_dom(|dom| match dom.clone_node(node, deep) {
    Ok(clone) => (Ok(clone), false),
    Err(err) => (Err(err), false),
  })
}

// -----------------------------------------------------------------------------------------------
// Node.textContent.

pub(crate) fn text_content_get_from_dom(dom: &crate::dom2::Document, node_id: NodeId) -> Option<String> {
  let root_node = dom.nodes().get(node_id.index())?;
  match &root_node.kind {
    NodeKind::Document { .. } | NodeKind::Doctype { .. } => None,
    NodeKind::Text { content } => Some(content.clone()),
    NodeKind::Comment { content } => Some(content.clone()),
    NodeKind::ProcessingInstruction { data, .. } => Some(data.clone()),
    NodeKind::Element { .. }
    | NodeKind::Slot { .. }
    | NodeKind::DocumentFragment
    | NodeKind::ShadowRoot { .. } => {
      let mut out = String::new();

      let mut remaining = dom.nodes_len().saturating_add(1);
      let mut stack: Vec<NodeId> = Vec::new();

      // Seed traversal with children in reverse so we pop in tree order.
      for &child in root_node.children.iter().rev() {
        if child.index() >= dom.nodes_len() {
          continue;
        }
        let Some(child_node) = dom.nodes().get(child.index()) else {
          continue;
        };
        if child_node.parent != Some(node_id) {
          continue;
        }
        // `ShadowRoot` is not part of the light DOM tree for `textContent` semantics.
        if matches!(&root_node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
          && matches!(&child_node.kind, NodeKind::ShadowRoot { .. })
        {
          continue;
        }
        stack.push(child);
      }

      while let Some(id) = stack.pop() {
        if remaining == 0 {
          break;
        }
        remaining -= 1;

        let Some(node) = dom.nodes().get(id.index()) else {
          continue;
        };
        if let NodeKind::Text { content } = &node.kind {
          out.push_str(content);
        }

        for &child in node.children.iter().rev() {
          if child.index() >= dom.nodes_len() {
            continue;
          }
          let Some(child_node) = dom.nodes().get(child.index()) else {
            continue;
          };
          if child_node.parent != Some(id) {
            continue;
          }
          if matches!(&node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
            && matches!(&child_node.kind, NodeKind::ShadowRoot { .. })
          {
            continue;
          }
          stack.push(child);
        }
      }

      Some(out)
    }
  }
}

/// `Node.textContent` getter.
///
/// Returns `None` for node types that produce `null` in the web platform (`Document`, `Doctype`).
pub fn text_content_get<Host: DomHost + ?Sized>(host: &Host, node_id: NodeId) -> Option<String> {
  host.with_dom(|dom| text_content_get_from_dom(dom, node_id))
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TextContentSetResult {
  pub changed: bool,
  pub render_affecting: bool,
  pub did_replace_children: bool,
}

pub(crate) fn text_content_set_from_dom(
  dom: &mut crate::dom2::Document,
  node_id: NodeId,
  value: &str,
) -> std::result::Result<TextContentSetResult, DomError> {
  #[derive(Clone, Copy)]
  enum TextContentTarget {
    Text,
    Comment,
    ProcessingInstruction,
    ReplaceChildren { preserve_shadow_roots: bool },
    NoOp,
  }

  let target = {
    let Some(node) = dom.nodes().get(node_id.index()) else {
      return Err(DomError::NotFoundError);
    };
    match &node.kind {
      NodeKind::Text { .. } => TextContentTarget::Text,
      NodeKind::Comment { .. } => TextContentTarget::Comment,
      NodeKind::ProcessingInstruction { .. } => TextContentTarget::ProcessingInstruction,
      NodeKind::Element { .. } | NodeKind::Slot { .. } => TextContentTarget::ReplaceChildren {
        preserve_shadow_roots: true,
      },
      NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => TextContentTarget::ReplaceChildren {
        preserve_shadow_roots: false,
      },
      NodeKind::Document { .. } | NodeKind::Doctype { .. } => TextContentTarget::NoOp,
    }
  };

  match target {
    TextContentTarget::Text => dom.set_text_data(node_id, value).map(|changed| TextContentSetResult {
      changed,
      render_affecting: changed,
      did_replace_children: false,
    }),
    TextContentTarget::Comment => dom.set_comment_data(node_id, value).map(|changed| TextContentSetResult {
      changed,
      // Comments do not affect rendering (they're ignored by renderer snapshots).
      render_affecting: false,
      did_replace_children: false,
    }),
    TextContentTarget::ProcessingInstruction => dom
      .set_processing_instruction_data(node_id, value)
      .map(|changed| TextContentSetResult {
        changed,
        // Processing instructions do not affect rendering.
        render_affecting: false,
        did_replace_children: false,
      }),
    TextContentTarget::ReplaceChildren {
      preserve_shadow_roots,
    } => {
      let mut changed = false;

      let children = dom.children(node_id)?.to_vec();
      for child in children {
        if child.index() >= dom.nodes_len() {
          continue;
        }
        let Some(child_node) = dom.nodes().get(child.index()) else {
          continue;
        };
        if child_node.parent != Some(node_id) {
          continue;
        }
        if preserve_shadow_roots && matches!(&child_node.kind, NodeKind::ShadowRoot { .. }) {
          continue;
        }
        changed |= dom.remove_child(node_id, child)?;
      }

      if !value.is_empty() {
        let text_node = dom.create_text(value);
        changed |= dom.append_child(node_id, text_node)?;
      }

      Ok(TextContentSetResult {
        changed,
        render_affecting: changed,
        did_replace_children: changed,
      })
    }
    TextContentTarget::NoOp => Ok(TextContentSetResult {
      changed: false,
      render_affecting: false,
      did_replace_children: false,
    }),
  }
}

/// `Node.textContent` setter.
///
/// Returns `Ok(true)` only when the DOM actually changes.
pub fn text_content_set<Host: DomHost + ?Sized>(
  host: &mut Host,
  node_id: NodeId,
  value: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| {
    match text_content_set_from_dom(dom, node_id, value) {
      Ok(result) => (Ok(result.changed), result.render_affecting),
      Err(err) => (Err(err), false),
    }
  })
}

/// `Element.classList.add(token)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying `class` attribute changes.
pub fn class_list_add<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  token: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.class_list_add(element, &[token]) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Element.classList.remove(token)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying `class` attribute changes.
pub fn class_list_remove<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  token: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.class_list_remove(element, &[token]) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Element.classList.toggle(token[, force])` for a `dom2` element.
///
/// Returns whether `token` is present after the operation. The host invalidation flag is derived by
/// comparing whether the token's presence changed.
pub fn class_list_toggle<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  token: &str,
  force: Option<bool>,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| {
    let before = match dom.class_list_contains(element, token) {
      Ok(v) => v,
      Err(err) => return (Err(err), false),
    };
    match dom.class_list_toggle(element, token, force) {
      Ok(after) => {
        let changed = after != before;
        (Ok(after), changed)
      }
      Err(err) => (Err(err), false),
    }
  })
}

/// `Element.classList.replace(token, newToken)` for a `dom2` element.
///
/// Returns whether `token` existed. The host invalidation flag is derived by comparing the `class`
/// attribute before/after the operation.
pub fn class_list_replace<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  token: &str,
  new_token: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| {
    let before = match dom.get_attribute(element, "class") {
      Ok(v) => v.map(str::to_string),
      Err(err) => return (Err(err), false),
    };

    match dom.class_list_replace(element, token, new_token) {
      Ok(found) => {
        let after = match dom.get_attribute(element, "class") {
          Ok(v) => v.map(str::to_string),
          Err(err) => return (Err(err), false),
        };
        let changed = before != after;
        (Ok(found), changed)
      }
      Err(err) => (Err(err), false),
    }
  })
}

/// `Element.getAttribute(name)` for a `dom2` element.
pub fn get_attribute<Host: DomHost + ?Sized>(
  host: &Host,
  element: NodeId,
  name: &str,
) -> std::result::Result<Option<String>, DomError> {
  host.with_dom(|dom| dom.get_attribute(element, name).map(|v| v.map(str::to_string)))
}

/// `Element.hasAttribute(name)` for a `dom2` element.
pub fn has_attribute<Host: DomHost + ?Sized>(
  host: &Host,
  element: NodeId,
  name: &str,
) -> std::result::Result<bool, DomError> {
  host.with_dom(|dom| dom.has_attribute(element, name))
}

/// `Element.getAttributeNames()` for a `dom2` element.
pub fn get_attribute_names<Host: DomHost + ?Sized>(
  host: &Host,
  element: NodeId,
) -> std::result::Result<Vec<String>, DomError> {
  host.with_dom(|dom| dom.attribute_names(element))
}

/// `Element.setAttribute(name, value)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying attribute list changes.
pub fn set_attribute<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  name: &str,
  value: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.set_attribute(element, name, value) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Element.removeAttribute(name)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying attribute list changes.
pub fn remove_attribute<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  name: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.remove_attribute(element, name) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}

/// `Element.dataset.<prop>` getter for a `dom2` element.
///
/// Invalid property names yield `None`.
pub fn dataset_get<Host: DomHost + ?Sized>(
  host: &Host,
  element: NodeId,
  prop: &str,
) -> Option<String> {
  host.with_dom(|dom| dom.dataset_get(element, prop).map(str::to_string))
}

/// `Element.dataset.<prop> = value` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying `data-*` attribute changes.
pub fn dataset_set<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  prop: &str,
  value: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.dataset_set(element, prop, value) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}

/// `delete Element.dataset.<prop>` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying `data-*` attribute changes.
pub fn dataset_delete<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  prop: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.dataset_delete(element, prop) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}

/// `CSSStyleDeclaration.getPropertyValue(name)` for a `dom2` element.
pub fn style_get_property_value<Host: DomHost + ?Sized>(
  host: &Host,
  element: NodeId,
  name: &str,
) -> String {
  host.with_dom(|dom| dom.style_get_property_value(element, name))
}

/// `CSSStyleDeclaration.setProperty(name, value)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying `style` attribute changes.
pub fn style_set_property<Host: DomHost + ?Sized>(
  host: &mut Host,
  element: NodeId,
  name: &str,
  value: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.style_set_property(element, name, value) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::host_document::HostDocumentState;

  #[test]
  fn text_content_get_text_node() {
    let root = crate::dom::parse_html("<!doctype html><div id=host>hello</div>").unwrap();
    let host = HostDocumentState::from_renderer_dom(&root);

    let host_el = host.dom().get_element_by_id("host").expect("host element");
    let text = host.dom().first_child(host_el).expect("text node");
    assert_eq!(text_content_get(&host, text), Some("hello".to_string()));
  }

  #[test]
  fn text_content_get_element_nested_text() {
    let root = crate::dom::parse_html("<!doctype html><div id=host>foo<span>bar</span>baz</div>").unwrap();
    let host = HostDocumentState::from_renderer_dom(&root);

    let host_el = host.dom().get_element_by_id("host").expect("host element");
    assert_eq!(text_content_get(&host, host_el), Some("foobarbaz".to_string()));
  }

  #[test]
  fn text_content_get_ignores_shadow_root_children() {
    let root = crate::dom::parse_html(
      "<!doctype html><div id=host>light<span id=gone>gone</span>\
       <template shadowroot=open><span id=shadow>shadow</span></template>tail</div>",
    )
    .unwrap();
    let host = HostDocumentState::from_renderer_dom(&root);

    let host_el = host.dom().get_element_by_id("host").expect("host element");
    assert_eq!(
      text_content_get(&host, host_el),
      Some("lightgonetail".to_string())
    );
  }

  #[test]
  fn text_content_set_preserves_shadow_root_children() {
    let root = crate::dom::parse_html(
      "<!doctype html><div id=host>light<span id=gone>gone</span>\
       <template shadowroot=open><span id=shadow>shadow</span></template>tail</div>",
    )
    .unwrap();
    let mut host = HostDocumentState::from_renderer_dom(&root);

    let host_el = host.dom().get_element_by_id("host").expect("host element");
    let gone_el = host.dom().get_element_by_id("gone").expect("gone element");

    let shadow_root = host.with_dom(|dom| {
      dom
        .node(host_el)
        .children
        .iter()
        .copied()
        .find(|&child| matches!(&dom.node(child).kind, NodeKind::ShadowRoot { .. }))
        .expect("expected ShadowRoot child")
    });

    assert!(text_content_set(&mut host, host_el, "new").unwrap());
    assert_eq!(text_content_get(&host, host_el), Some("new".to_string()));

    // Shadow root child should still be parented to the host element.
    assert_eq!(parent_node(&host, shadow_root), Some(host_el));
    assert_eq!(text_content_get(&host, shadow_root), Some("shadow".to_string()));

    // Non-shadow-root children should be detached.
    assert_eq!(parent_node(&host, gone_el), None);
  }

  #[test]
  fn text_content_set_on_document_is_no_op() {
    let root = crate::dom::parse_html("<!doctype html><div id=host>hello</div>").unwrap();
    let mut host = HostDocumentState::from_renderer_dom(&root);

    let host_el = host.dom().get_element_by_id("host").expect("host element");
    let doc = host.dom().root();
    assert!(!text_content_set(&mut host, doc, "new").unwrap());
    assert_eq!(text_content_get(&host, host_el), Some("hello".to_string()));
  }

  #[test]
  fn element_traversal_skips_text_nodes() {
    let root = crate::dom::parse_html(
      "<!doctype html><div id=host><span id=a></span>text<p id=b></p></div>",
    )
    .unwrap();
    let host = HostDocumentState::from_renderer_dom(&root);

    let host_el = host.dom().get_element_by_id("host").expect("host element");
    let a = host.dom().get_element_by_id("a").expect("a element");
    let b = host.dom().get_element_by_id("b").expect("b element");

    assert_eq!(first_element_child(&host, host_el), Some(a));
    assert_eq!(last_element_child(&host, host_el), Some(b));
    assert_eq!(next_element_sibling(&host, a), Some(b));
    assert_eq!(previous_element_sibling(&host, b), Some(a));
    assert_eq!(child_element_count(&host, host_el), 2);
    assert_eq!(children_elements(&host, host_el), vec![a, b]);
  }

  #[test]
  fn get_attribute_helpers_work_for_html_elements() {
    let root =
      crate::dom::parse_html("<!doctype html><div id=host class='a b' data-x='y'></div>").unwrap();
    let host = HostDocumentState::from_renderer_dom(&root);

    let host_el = host.dom().get_element_by_id("host").expect("host element");
    assert_eq!(
      get_attribute(&host, host_el, "ID").unwrap(),
      Some("host".to_string())
    );
    assert_eq!(get_attribute(&host, host_el, "missing").unwrap(), None);
    assert!(has_attribute(&host, host_el, "class").unwrap());
    assert!(!has_attribute(&host, host_el, "missing").unwrap());

    let names = get_attribute_names(&host, host_el).unwrap();
    assert!(names.contains(&"id".to_string()));
    assert!(names.contains(&"class".to_string()));
    assert!(names.contains(&"data-x".to_string()));
  }

  #[test]
  fn selector_helpers_work() {
    let root = crate::dom::parse_html(
      "<!doctype html><div id=host><span id=a class='foo bar'></span></div>",
    )
    .unwrap();
    let mut host = HostDocumentState::from_renderer_dom(&root);

    let a = host.dom().get_element_by_id("a").expect("a element");
    let host_el = host.dom().get_element_by_id("host").expect("host element");

    assert!(matches_selector(&mut host, a, ".foo").unwrap());
    assert!(!matches_selector(&mut host, a, "#host").unwrap());
    assert_eq!(closest(&mut host, a, "span").unwrap(), Some(a));
    assert_eq!(closest(&mut host, a, "div").unwrap(), Some(host_el));

    assert!(matches_selector(&mut host, a, "???").is_err());
    assert!(closest(&mut host, a, "???").is_err());
  }

  #[test]
  fn get_elements_by_helpers_work() {
    let root = crate::dom::parse_html(
      "<!doctype html><div id=outer class='foo bar'><span id=a class=foo></span></div>",
    )
    .unwrap();
    let host = HostDocumentState::from_renderer_dom(&root);
    let doc = host.dom().root();

    let outer = host.dom().get_element_by_id("outer").expect("outer element");
    let a = host.dom().get_element_by_id("a").expect("a element");

    assert_eq!(get_elements_by_tag_name(&host, doc, "div"), vec![outer]);
    assert_eq!(get_elements_by_class_name(&host, doc, "foo"), vec![outer, a]);
    assert_eq!(get_elements_by_class_name(&host, doc, "foo bar"), vec![outer]);
  }
}
