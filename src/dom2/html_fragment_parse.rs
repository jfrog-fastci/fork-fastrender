use crate::dom::HTML_NAMESPACE;
use crate::error::RenderStage;

use html5ever::parse_fragment;
use html5ever::tendril::StrTendril;
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::QuirksMode as HtmlQuirksMode;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::Attribute;
use html5ever::ParseOpts;
use markup5ever::LocalName;
use markup5ever::Namespace;
use markup5ever::QualName;
use selectors::context::QuirksMode;
use std::io;

use super::{Document, Dom2TreeSink, DomError, NodeId, NodeKind};

const DOM_PARSE_READ_DEADLINE_STRIDE: usize = 1;
const DOM_PARSE_READ_MAX_CHUNK_BYTES: usize = 16 * 1024;

struct DeadlineCheckedRead<R> {
  inner: R,
  deadline_counter: usize,
}

impl<R> DeadlineCheckedRead<R> {
  fn new(inner: R) -> Self {
    Self {
      inner,
      deadline_counter: 0,
    }
  }
}

impl<R: io::Read> io::Read for DeadlineCheckedRead<R> {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    if let Err(err) = crate::render_control::check_active_periodic(
      &mut self.deadline_counter,
      DOM_PARSE_READ_DEADLINE_STRIDE,
      RenderStage::DomParse,
    ) {
      return Err(io::Error::new(io::ErrorKind::TimedOut, err));
    }
    let len = buf.len().min(DOM_PARSE_READ_MAX_CHUNK_BYTES);
    self.inner.read(&mut buf[..len])
  }
}

fn map_quirks_mode_to_html(mode: QuirksMode) -> HtmlQuirksMode {
  match mode {
    QuirksMode::Quirks => HtmlQuirksMode::Quirks,
    QuirksMode::LimitedQuirks => HtmlQuirksMode::LimitedQuirks,
    QuirksMode::NoQuirks => HtmlQuirksMode::NoQuirks,
  }
}

fn normalize_parse_namespace(namespace: &str) -> &str {
  if namespace.is_empty() {
    return HTML_NAMESPACE;
  }
  namespace
}

fn fragment_context_qual_name(context_tag: &str, context_namespace: &str) -> QualName {
  let ns = normalize_parse_namespace(context_namespace);
  let ns: Namespace = ns.into();

  // HTML local names are ASCII-lowercased by the HTML parser. Use the same normalization for the
  // fragment context to match browser behavior.
  let local: LocalName = if ns.as_ref() == HTML_NAMESPACE {
    context_tag.to_ascii_lowercase().into()
  } else {
    context_tag.into()
  };
  QualName::new(None, ns, local)
}

fn build_context_attrs(context_namespace: &str, attrs: &[(String, String)]) -> Vec<Attribute> {
  if attrs.is_empty() {
    return Vec::new();
  }

  // In HTML contexts, attribute names are ASCII case-insensitive and are normalized to lowercase by
  // the parser. Preserve case for foreign contexts (MathML/SVG), where attribute names can be
  // case-sensitive.
  let is_html = normalize_parse_namespace(context_namespace) == HTML_NAMESPACE;
  let attr_ns: Namespace = "".into();

  attrs
    .iter()
    .map(|(name, value)| {
      let local: LocalName = if is_html {
        name.to_ascii_lowercase().into()
      } else {
        name.as_str().into()
      };
      Attribute {
        name: QualName::new(None, attr_ns.clone(), local),
        value: StrTendril::from(value.as_str()),
      }
    })
    .collect()
}

fn connected_children(doc: &Document, parent: NodeId) -> Vec<NodeId> {
  doc
    .node(parent)
    .children
    .iter()
    .copied()
    .filter(|&child| doc.nodes().get(child.index()).is_some_and(|n| n.parent == Some(parent)))
    .collect()
}

fn is_synthetic_html_wrapper(doc: &Document, node_id: NodeId) -> bool {
  match &doc.node(node_id).kind {
    NodeKind::Element {
      tag_name, namespace, ..
    } => (namespace.is_empty() || namespace == HTML_NAMESPACE) && tag_name.eq_ignore_ascii_case("html"),
    _ => false,
  }
}

fn fragment_roots_from_document(doc: &Document) -> Vec<NodeId> {
  let root = doc.root();
  let children = connected_children(doc, root);

  let non_trivia: Vec<NodeId> = children
    .iter()
    .copied()
    .filter(|&id| !matches!(&doc.node(id).kind, NodeKind::Doctype { .. } | NodeKind::Comment { .. }))
    .collect();

  if non_trivia.len() == 1 && is_synthetic_html_wrapper(doc, non_trivia[0]) {
    let html_wrapper = non_trivia[0];
    let mut out = Vec::new();
    for child in children {
      match &doc.node(child).kind {
        NodeKind::Doctype { .. } => continue,
        _ if child == html_wrapper => {
          for grandchild in connected_children(doc, html_wrapper) {
            if matches!(&doc.node(grandchild).kind, NodeKind::Doctype { .. }) {
              continue;
            }
            out.push(grandchild);
          }
        }
        _ => out.push(child),
      }
    }
    return out;
  }

  // HTML DOM Parsing: DOCTYPE is not a valid fragment child; if the parser produced one, drop it.
  children
    .into_iter()
    .filter(|&id| !matches!(&doc.node(id).kind, NodeKind::Doctype { .. }))
    .collect()
}

pub(super) struct ParsedFragment {
  pub(super) document: Document,
  pub(super) roots: Vec<NodeId>,
}

/// Parse an HTML fragment into a temporary `dom2::Document`.
///
/// The returned roots are `NodeId`s into the temporary document.
pub(super) fn parse_html_fragment_to_dom2_document(
  html: &str,
  context_tag: &str,
  context_namespace: &str,
  context_attributes: &[(String, String)],
  scripting_enabled: bool,
  document_quirks: QuirksMode,
) -> Result<ParsedFragment, DomError> {
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled,
      quirks_mode: map_quirks_mode_to_html(document_quirks),
      ..Default::default()
    },
    ..Default::default()
  };

  let context_name = fragment_context_qual_name(context_tag, context_namespace);
  let context_attrs = build_context_attrs(context_namespace, context_attributes);

  // `html5ever::parse_fragment` requires `context_element_allows_scripting` as a separate flag from
  // `TreeBuilderOpts::scripting_enabled`. It is used to determine the tokenizer initial state for
  // `<noscript>` contexts; wire it through from `scripting_enabled` so fragment parsing matches
  // browser `innerHTML` semantics.
  let context_element_allows_scripting = scripting_enabled;

  let reader = io::Cursor::new(html.as_bytes());
  let mut reader = DeadlineCheckedRead::new(reader);
  let mut document = parse_fragment(
    Dom2TreeSink::new_for_fragment(/* document_url */ None),
    opts,
    context_name,
    context_attrs,
    context_element_allows_scripting,
  )
  .from_utf8()
  .read_from(&mut reader)
  .map_err(|_| DomError::SyntaxError)?;
  document.scripting_enabled = scripting_enabled;

  // The TreeSink performs a best-effort attachment pass in `finish()`, but keep this explicit so
  // fragment parsing stays consistent if the sink implementation changes.
  document.attach_shadow_roots();

  let roots = fragment_roots_from_document(&document);
  Ok(ParsedFragment { document, roots })
}

