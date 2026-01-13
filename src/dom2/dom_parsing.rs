use selectors::context::QuirksMode;

use super::html_fragment_parse::parse_html_fragment_to_dom2_document;
use super::import::import_dom2_nodes_into_parent;
use super::{Document, DomError, NodeId, NodeKind};

fn document_quirks_mode(doc: &Document) -> QuirksMode {
  match &doc.nodes[doc.root().index()].kind {
    NodeKind::Document { quirks_mode } => *quirks_mode,
    _ => QuirksMode::NoQuirks,
  }
}

fn element_context(
  doc: &Document,
  context: NodeId,
) -> Result<(&str, &str, Vec<(String, String)>), DomError> {
  let node = doc
    .nodes
    .get(context.index())
    .ok_or(DomError::NotFoundError)?;
  match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } => Ok((
      tag_name.as_str(),
      namespace.as_str(),
      attributes
        .iter()
        .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
        .collect(),
    )),
    // `NodeKind::Slot` does not store its tag name, but today it always represents `<slot>`.
    NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => Ok((
      "slot",
      namespace.as_str(),
      attributes
        .iter()
        .map(|attr| (attr.qualified_name().into_owned(), attr.value.clone()))
        .collect(),
    )),
    _ => Err(DomError::InvalidNodeTypeError),
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
  let (context_tag, context_namespace, context_attributes) = element_context(doc, context)?;
  let quirks_mode = document_quirks_mode(doc);

  let parsed = parse_html_fragment_to_dom2_document(
    html,
    context_tag,
    context_namespace,
    context_attributes.as_slice(),
    doc.scripting_enabled,
    quirks_mode,
  )
  ?;

  let fragment = doc.create_document_fragment();
  let imported_roots = import_dom2_nodes_into_parent(doc, fragment, &parsed.document, &parsed.roots);

  // HTML: elements created by `innerHTML`/`outerHTML` parsing must not execute scripts when
  // inserted into the document.
  //
  // The platform uses a per-script-element "already started" flag to ensure scripts created by
  // dynamic markup insertion do not execute. FastRender mirrors this flag via
  // `dom2::Node::script_already_started`.
  let mut to_mark: Vec<NodeId> = Vec::new();
  for root in imported_roots {
    to_mark.extend(doc.subtree_preorder(root));
  }
  for node_id in to_mark {
    let NodeKind::Element {
      tag_name,
      namespace,
      ..
    } = &doc.nodes[node_id.index()].kind
    else {
      continue;
    };
    if !tag_name.eq_ignore_ascii_case("script") {
      continue;
    }
    if !doc.is_html_case_insensitive_namespace(namespace) {
      continue;
    }
    doc.nodes[node_id.index()].script_force_async = false;
    doc.nodes[node_id.index()].script_parser_document = false;
    doc.nodes[node_id.index()].script_already_started = true;
  }

  Ok(fragment)
}
