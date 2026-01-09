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

/// `Document.documentElement` (returns the `<html>` element for HTML documents).
pub fn document_element<Host: DomHost>(host: &Host) -> Option<NodeId> {
  host.with_dom(|dom| dom.document_element())
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
