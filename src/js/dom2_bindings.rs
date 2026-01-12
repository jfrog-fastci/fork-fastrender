//! Minimal DOM (WHATWG DOM-ish) helpers used by the JavaScript binding layer.
//!
//! These bindings are intentionally scoped: they provide enough DOM mutation surface to exercise
//! the renderer invalidation plumbing in `BrowserDocumentDom2`/`BrowserDocument2`.
//!
//! The key design point is that DOM bindings should **not** mutate `dom2::Document` directly. All
//! mutations must go through [`crate::js::DomHost`] so the host can coalesce invalidation and avoid
//! re-rendering when an operation is a no-op.

use crate::dom2::{DomError, NodeId, NodeKind};
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

/// `Node.lastChild`.
pub fn last_child<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.last_child(node))
}

/// `Node.previousSibling`.
pub fn previous_sibling<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.previous_sibling(node))
}

/// `Node.nextSibling`.
pub fn next_sibling<Host: DomHost + ?Sized>(host: &Host, node: NodeId) -> Option<NodeId> {
  host.with_dom(|dom| dom.next_sibling(node))
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
}
