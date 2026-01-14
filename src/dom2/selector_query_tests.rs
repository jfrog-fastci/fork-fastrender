#![cfg(test)]

use crate::dom::parse_html;
use crate::dom::{DomNode, DomNodeType, ShadowRootMode};
use crate::web::dom::DomException;
use selectors::context::QuirksMode;

use super::{Attribute, Document, NodeId, NodeKind, SlotAssignmentMode};

fn attr_value<'a>(doc: &'a Document, node: NodeId, name: &str) -> Option<&'a str> {
  let node = doc.node(node);
  let (namespace, attrs) = match &node.kind {
    NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => (namespace, attributes),
    _ => return None,
  };
  let is_html = doc.is_html_case_insensitive_namespace(namespace);
  attrs
    .iter()
    .find(|attr| attr.qualified_name_matches(name, is_html))
    .map(|attr| attr.value.as_str())
}

fn find_element_by_id(doc: &Document, id: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => {
        let is_html = doc.is_html_case_insensitive_namespace(namespace);
        attributes
          .iter()
          .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
          .then_some(NodeId(idx))
      }
      _ => None,
    })
    .unwrap_or_else(|| panic!("element with id={id:?} not found"))
}

fn find_inert_descendant_with_class(doc: &Document, class: &str) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => {
        let is_html = doc.is_html_case_insensitive_namespace(namespace);
        let has_class = attributes.iter().any(|attr| {
          attr.qualified_name_matches("class", is_html)
            && attr
              .value
              .split_ascii_whitespace()
              .any(|c| c == class)
        });
        let id = NodeId(idx);
        (has_class && doc.is_descendant_of_inert_template(id)).then_some(id)
      }
      _ => None,
    })
    .unwrap_or_else(|| panic!("inert descendant with class={class:?} not found"))
}

#[test]
fn query_selector_skips_inert_template_contents() {
  let root = parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <template><div class=x></div></template>
        <div class=x id=out></div>
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let inert_div = find_inert_descendant_with_class(&doc, "x");
  assert!(
    doc.is_descendant_of_inert_template(inert_div),
    "expected test fixture to contain an inert template descendant"
  );

  let result = doc
    .query_selector(".x", None)
    .unwrap()
    .expect("expected a match for .x");
  assert_eq!(
    attr_value(&doc, result, "id"),
    Some("out"),
    "query_selector should skip inert template descendants"
  );
}

#[test]
fn invalid_selector_returns_syntax_error() {
  let root = parse_html(r#"<!doctype html><div></div>"#).unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let err = doc.query_selector("div[", None).unwrap_err();
  assert!(matches!(err, DomException::SyntaxError { .. }));
}

#[test]
fn query_selector_scope_limits_to_subtree_and_detached_scope_returns_none() {
  let root = parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <div id=scope>
          <div class=x id=in></div>
        </div>
        <div class=x id=out></div>
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let scope = find_element_by_id(&doc, "scope");

  let result = doc
    .query_selector(".x", Some(scope))
    .unwrap()
    .expect("expected a scoped match");
  assert_eq!(attr_value(&doc, result, "id"), Some("in"));

  let all = doc.query_selector_all(".x", Some(scope)).unwrap();
  assert_eq!(all.len(), 1);
  assert_eq!(attr_value(&doc, all[0], "id"), Some("in"));

  // Detached scopes with no matching descendants should return no matches.
  let detached = doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: "".to_string(),
      prefix: None,
      attributes: Vec::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  assert_eq!(doc.query_selector(".x", Some(detached)).unwrap(), None);
  assert!(doc
    .query_selector_all(".x", Some(detached))
    .unwrap()
    .is_empty());

  // Detached subtrees should still be queryable.
  let detached_child = doc.push_node(
    NodeKind::Element {
      tag_name: "div".to_string(),
      namespace: "".to_string(),
      prefix: None,
      attributes: vec![Attribute::new_no_namespace("class", "x")],
    },
    Some(detached),
    /* inert_subtree */ false,
  );
  assert_eq!(
    doc.query_selector(".x", Some(detached)).unwrap(),
    Some(detached_child)
  );
}

#[test]
fn matches_selector_returns_false_for_non_elements_and_inert_template_descendants() {
  let root = parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <template><div class=x></div></template>
        <div class=x id=out></div>
        text
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  assert!(
    !doc.matches_selector(doc.root(), ".x").unwrap(),
    "document node should never match selectors"
  );

  let text_node = doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| matches!(node.kind, NodeKind::Text { .. }).then_some(NodeId(idx)))
    .expect("expected a text node");
  assert!(
    !doc.matches_selector(text_node, "div").unwrap(),
    "text nodes should never match selectors"
  );

  let inert_div = find_inert_descendant_with_class(&doc, "x");
  assert!(doc.is_descendant_of_inert_template(inert_div));
  assert!(
    doc.matches_selector(inert_div, ".x").unwrap(),
    "inert template descendants should still match selectors when queried directly"
  );

  let out_div = find_element_by_id(&doc, "out");
  assert!(
    doc.matches_selector(out_div, ".x").unwrap(),
    "sanity: non-inert elements should still match selectors"
  );
}

#[test]
fn query_selector_handles_wbr_synthetic_zwsp_nodes_and_scope() {
  // Build a minimal renderer DOM tree with a single `<wbr>` element.
  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "wbr".to_string(),
        namespace: "".to_string(),
        attributes: vec![("id".to_string(), "w".to_string())],
      },
      children: Vec::new(),
    }],
  };
  let mut doc = Document::from_renderer_dom(&root);

  let wbr = doc.get_element_by_id("w").expect("missing `<wbr>` element");

  let all = doc.query_selector_all("wbr", None).unwrap();
  assert_eq!(all, vec![wbr]);
  assert_eq!(doc.query_selector("wbr", None).unwrap(), Some(wbr));

  // Verify that `:scope` anchors to the provided scoping root and that the root itself participates
  // in matching.
  assert_eq!(doc.query_selector(":scope", Some(wbr)).unwrap(), Some(wbr));
  assert_eq!(
    doc.query_selector_all(":scope", Some(wbr)).unwrap(),
    vec![wbr]
  );
}

#[test]
fn query_selector_supports_virtual_scoping_roots_for_document_fragments() {
  // Selectors4 defines a virtual scoping root for document fragments. The virtual root cannot be
  // the subject of the selector (`:scope` alone matches nothing), but it acts as the parent of any
  // top-level elements in the fragment so relationship selectors like `:scope > span` work.
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let frag = doc.create_document_fragment();
  let span = doc.create_element("span", "");
  doc.append_child(frag, span).unwrap();

  assert_eq!(doc.query_selector("span", Some(frag)).unwrap(), Some(span));
  assert_eq!(
    doc.query_selector(":scope > span", Some(frag)).unwrap(),
    Some(span)
  );
  assert_eq!(doc.query_selector(":scope", Some(frag)).unwrap(), None);
  assert!(doc
    .query_selector_all(":scope", Some(frag))
    .unwrap()
    .is_empty());
  assert_eq!(doc.query_selector("* > span", Some(frag)).unwrap(), None);
}

#[test]
fn query_selector_supports_virtual_scoping_roots_for_shadow_roots() {
  // Selectors4 defines `:scope` for shadow roots as a virtual scoping root. Similar to document
  // fragments, the virtual root cannot be the subject of the selector, but it is treated as the
  // parent of the shadow root's top-level elements for relationship selectors.
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let host = doc.create_element("div", "");
  doc.append_child(doc.root(), host).unwrap();

  let shadow_root = doc.push_node(
    NodeKind::ShadowRoot {
      mode: ShadowRootMode::Open,
      delegates_focus: false,
      slot_assignment: SlotAssignmentMode::Named,
      clonable: false,
      serializable: false,
      declarative: false,
    },
    Some(host),
    /* inert_subtree */ false,
  );
  let span = doc.create_element("span", "");
  doc.append_child(shadow_root, span).unwrap();

  assert_eq!(
    doc.query_selector("span", Some(shadow_root)).unwrap(),
    Some(span)
  );
  assert_eq!(
    doc
      .query_selector(":scope > span", Some(shadow_root))
      .unwrap(),
    Some(span)
  );
  assert_eq!(
    doc.query_selector(":scope", Some(shadow_root)).unwrap(),
    None
  );
  assert!(doc
    .query_selector_all(":scope", Some(shadow_root))
    .unwrap()
    .is_empty());
  assert_eq!(
    doc.query_selector("* > span", Some(shadow_root)).unwrap(),
    None
  );
}

#[test]
fn query_selector_ignores_nodes_detached_via_parent_pointer_only() {
  // Dom mutation logic can temporarily leave stale entries in a parent's children list. Ensure we
  // treat the parent pointer as authoritative and do not surface detached nodes via selector APIs.
  let root = parse_html(
    r#"<!doctype html>
    <html>
      <body>
        <div id=host><div class=x id=in></div></div>
        <div class=x id=out></div>
      </body>
    </html>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&root);

  let in_div = find_element_by_id(&doc, "in");
  // Detach by severing the parent pointer, but leave the stale entry in the original parent's
  // children list.
  doc.node_mut(in_div).parent = None;

  assert!(
    !doc.is_connected(in_div),
    "sanity: severing the parent pointer should disconnect the node"
  );

  let result = doc
    .query_selector(".x", None)
    .unwrap()
    .expect("expected a match for .x");
  assert_eq!(
    attr_value(&doc, result, "id"),
    Some("out"),
    "query_selector should ignore nodes detached by parent pointer mismatch"
  );

  let all = doc.query_selector_all(".x", None).unwrap();
  assert_eq!(all.len(), 1);
  assert_eq!(attr_value(&doc, all[0], "id"), Some("out"));

  assert!(
    doc.matches_selector(in_div, ".x").unwrap(),
    "matches_selector should still work for detached nodes"
  );
}
