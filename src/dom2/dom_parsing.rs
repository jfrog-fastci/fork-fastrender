use crate::dom::HTML_NAMESPACE;
use crate::web::dom::DomException;
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{parse_fragment, ParseOpts};
use markup5ever::{LocalName, Namespace, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use super::{Document, NodeId, NodeKind};

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn context_qual_name(doc: &Document, context: NodeId) -> Result<QualName, DomException> {
  let (tag_name, namespace) = match &doc
    .nodes
    .get(context.index())
    .ok_or_else(|| DomException::syntax_error("Invalid context node id"))?
    .kind
  {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => (tag_name.as_str(), namespace.as_str()),
    // `NodeKind::Slot` does not store its tag name, but today it always represents `<slot>`.
    NodeKind::Slot { namespace, .. } => ("slot", namespace.as_str()),
    _ => {
      return Err(DomException::syntax_error(
        "Context node must be an element",
      ));
    }
  };

  let is_html = is_html_namespace(namespace);
  let ns: Namespace = if is_html {
    // Ensure HTML namespace even when stored as the empty string.
    markup5ever::ns!(html)
  } else {
    Namespace::from(namespace.to_string())
  };

  let local: LocalName = if is_html {
    LocalName::from(tag_name.to_ascii_lowercase())
  } else {
    LocalName::from(tag_name.to_string())
  };

  Ok(QualName::new(None, ns, local))
}

fn qual_name_to_attr_name(name: &QualName) -> String {
  match name.prefix.as_ref() {
    Some(prefix) => format!("{}:{}", prefix, name.local),
    None => name.local.to_string(),
  }
}

fn handle_children(handle: &Handle) -> Vec<Handle> {
  handle.children.borrow().iter().cloned().collect()
}

fn fragment_children_from_rcdom(rcdom: &RcDom) -> Vec<Handle> {
  let children = handle_children(&rcdom.document);

  // `html5ever`'s RcDom fragment parsing currently returns a synthetic `<html>` element as the sole
  // significant child of the document, with the actual fragment nodes as its children.
  //
  // Strip that wrapper so callers can insert the returned nodes directly as the result of
  // `Element.innerHTML` / `Element.outerHTML` parsing.
  let significant: Vec<Handle> = children
    .iter()
    .filter(|handle| !matches!(handle.data, NodeData::Doctype { .. } | NodeData::Comment { .. }))
    .cloned()
    .collect();

  if significant.len() == 1 {
    if let NodeData::Element { name, .. } = &significant[0].data {
      if name.ns.to_string() == HTML_NAMESPACE && name.local.to_string().eq_ignore_ascii_case("html") {
        return handle_children(&significant[0]);
      }
    }
  }

  significant
}

/// Parse an HTML fragment in the context of `context`, returning the top-level nodes as detached
/// [`NodeId`]s (their `parent` pointers are `None`).
pub(super) fn parse_html_fragment(
  doc: &mut Document,
  context: NodeId,
  html: &str,
) -> Result<Vec<NodeId>, DomException> {
  let context_name = context_qual_name(doc, context)?;

  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      // FastRender defaults to "scripting disabled" parsing semantics for static rendering.
      scripting_enabled: false,
      ..TreeBuilderOpts::default()
    },
    ..ParseOpts::default()
  };

  // `html5ever::parse_fragment` takes `context_element_allows_scripting` as a separate boolean flag
  // (it only affects the tokenizer initial state when the context element is `<noscript>`).
  // Keep it in sync with the tree builder scripting flag so `innerHTML`/`outerHTML` parsing matches
  // `parse_html_with_options`.
  let context_element_allows_scripting = opts.tree_builder.scripting_enabled;
  let rcdom: RcDom =
    parse_fragment(
      RcDom::default(),
      opts,
      context_name,
      Vec::new(),
      context_element_allows_scripting,
    )
    .one(html);

  let mut roots: Vec<NodeId> = Vec::new();

  #[derive(Clone)]
  struct WorkItem {
    parent: Option<NodeId>,
    handle: Handle,
  }

  // Seed stack with fragment root children (reverse so we pop in document order).
  let mut stack: Vec<WorkItem> = fragment_children_from_rcdom(&rcdom)
    .into_iter()
    .rev()
    .map(|handle| WorkItem { parent: None, handle })
    .collect();

  while let Some(item) = stack.pop() {
    match &item.handle.data {
      NodeData::Document => {
        // Document/fragment container: descend without creating a node.
        for child in handle_children(&item.handle).into_iter().rev() {
          stack.push(WorkItem {
            parent: item.parent,
            handle: child,
          });
        }
      }

      NodeData::Text { contents } => {
        let content = contents.borrow().to_string();
        let id = doc.push_node(
          NodeKind::Text { content },
          item.parent,
          /* inert_subtree */ false,
        );
        if item.parent.is_none() {
          roots.push(id);
        }
      }

      NodeData::Element {
        name,
        attrs,
        template_contents,
        ..
      } => {
        let tag_name = name.local.to_string();
        let mut namespace = name.ns.to_string();
        // Normalise HTML namespace to the empty string, matching the renderer DOM representation.
        if namespace == HTML_NAMESPACE {
          namespace.clear();
        }
        let is_html = is_html_namespace(&namespace);

        let attributes: Vec<(String, String)> = attrs
          .borrow()
          .iter()
          .map(|attr| {
            (
              qual_name_to_attr_name(&attr.name),
              attr.value.to_string(),
            )
          })
          .collect();

        let inert_subtree = tag_name.eq_ignore_ascii_case("template");
        let kind = if is_html && tag_name.eq_ignore_ascii_case("slot") {
          NodeKind::Slot {
            namespace: namespace.clone(),
            attributes,
            assigned: false,
          }
        } else {
          NodeKind::Element {
            tag_name: tag_name.clone(),
            namespace: namespace.clone(),
            attributes,
          }
        };

        let id = doc.push_node(kind, item.parent, inert_subtree);
        if item.parent.is_none() {
          roots.push(id);
        }

        // If this is a template element, use its template contents handle for children.
        let template_handle = template_contents.borrow();
        let children = if inert_subtree {
          template_handle
            .as_ref()
            .map(handle_children)
            .unwrap_or_else(|| handle_children(&item.handle))
        } else {
          handle_children(&item.handle)
        };
        drop(template_handle);

        for child in children.into_iter().rev() {
          stack.push(WorkItem {
            parent: Some(id),
            handle: child,
          });
        }
      }

      // Intentionally ignore unsupported node types (comment, doctype, etc).
      _ => {}
    }
  }

  Ok(roots)
}
