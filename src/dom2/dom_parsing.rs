use crate::dom::DomParseOptions;
use selectors::context::QuirksMode;

use super::import::import_domnodes_into_parent;
use super::{Document, DomError, NodeId, NodeKind};

fn document_quirks_mode(doc: &Document) -> QuirksMode {
  match &doc.nodes[doc.root().index()].kind {
    NodeKind::Document { quirks_mode } => *quirks_mode,
    _ => QuirksMode::NoQuirks,
  }
}

fn element_context(doc: &Document, context: NodeId) -> Result<(&str, &str), DomError> {
  let node = doc.nodes.get(context.index()).ok_or(DomError::NotFoundError)?;
  match &node.kind {
    NodeKind::Element {
      tag_name, namespace, ..
    } => Ok((tag_name.as_str(), namespace.as_str())),
    // `NodeKind::Slot` does not store its tag name, but today it always represents `<slot>`.
    NodeKind::Slot { namespace, .. } => Ok(("slot", namespace.as_str())),
    _ => Err(DomError::InvalidNodeType),
  }
}

/// Parse an HTML fragment in the context of `context`, importing the result into a fresh
/// `DocumentFragment` node.
///
/// The returned fragment is detached (its `parent` pointer is `None`). Callers should insert it
/// using `append_child` / `insert_before` / `replace_child` so `DocumentFragment` insertion semantics
/// are respected.
pub(super) fn parse_html_fragment_as_fragment(
  doc: &mut Document,
  context: NodeId,
  html: &str,
) -> Result<NodeId, DomError> {
  let (context_tag, context_namespace) = element_context(doc, context)?;
  let quirks_mode = document_quirks_mode(doc);

  let options = DomParseOptions::with_scripting_enabled(doc.scripting_enabled);
  let parsed =
    crate::dom::parse_html_fragment(html, context_tag, context_namespace, options, quirks_mode)
      .map_err(|_| DomError::SyntaxError)?;

  let fragment = doc.create_document_fragment();
  import_domnodes_into_parent(doc, fragment, &parsed);
  Ok(fragment)
}
