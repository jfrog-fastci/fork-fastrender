//! Minimal DOM (WHATWG DOM-ish) helpers used by the JavaScript binding layer.
//!
//! These bindings are intentionally scoped: they provide enough DOM mutation surface to exercise
//! the renderer invalidation plumbing in `BrowserDocumentDom2`/`BrowserDocument2`.
//!
//! The key design point is that DOM bindings should **not** mutate `dom2::Document` directly. All
//! mutations must go through [`crate::js::DomHost`] so the host can coalesce invalidation and avoid
//! re-rendering when an operation is a no-op.

use crate::dom2::{DomError, NodeId};
use crate::js::DomHost;
use crate::web::dom::DomException;

/// `Document.documentElement` (returns the `<html>` element for HTML documents).
pub fn document_element<Host: DomHost>(host: &Host) -> Option<NodeId> {
  host.with_dom(|dom| dom.document_element())
}

/// `Document.getElementById(id)`.
pub fn get_element_by_id<Host: DomHost>(host: &Host, id: &str) -> Option<NodeId> {
  host.with_dom(|dom| dom.get_element_by_id(id))
}

/// `ParentNode.querySelector(selectors)` for a `dom2` document.
///
/// This uses `dom2`'s selector matching engine, including inert `<template>` behaviour.
///
/// Note: `dom2::Document::query_selector` requires `&mut self` (it snapshots into renderer DOM
/// structures for selector matching), so this is routed through [`DomHost::mutate_dom`] but always
/// reports `changed=false`.
pub fn query_selector<Host: DomHost>(
  host: &mut Host,
  selectors: &str,
  scope: Option<NodeId>,
) -> std::result::Result<Option<NodeId>, DomException> {
  host.mutate_dom(|dom| (dom.query_selector(selectors, scope), false))
}

/// `ParentNode.querySelectorAll(selectors)` for a `dom2` document.
///
/// See [`query_selector`] for notes on DOM mutation tracking.
pub fn query_selector_all<Host: DomHost>(
  host: &mut Host,
  selectors: &str,
  scope: Option<NodeId>,
) -> std::result::Result<Vec<NodeId>, DomException> {
  host.mutate_dom(|dom| (dom.query_selector_all(selectors, scope), false))
}

/// `Element.classList.add(token)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying `class` attribute changes.
pub fn class_list_add<Host: DomHost>(
  host: &mut Host,
  element: NodeId,
  token: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| {
    match dom.class_list_add(element, &[token]) {
      Ok(changed) => (Ok(changed), changed),
      Err(err) => (Err(err), false),
    }
  })
}

/// `Element.classList.remove(token)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying `class` attribute changes.
pub fn class_list_remove<Host: DomHost>(
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
pub fn class_list_toggle<Host: DomHost>(
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

/// `Element.setAttribute(name, value)` for a `dom2` element.
///
/// Returns `Ok(true)` only when the underlying attribute list changes.
pub fn set_attribute<Host: DomHost>(
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
pub fn remove_attribute<Host: DomHost>(
  host: &mut Host,
  element: NodeId,
  name: &str,
) -> std::result::Result<bool, DomError> {
  host.mutate_dom(|dom| match dom.remove_attribute(element, name) {
    Ok(changed) => (Ok(changed), changed),
    Err(err) => (Err(err), false),
  })
}
