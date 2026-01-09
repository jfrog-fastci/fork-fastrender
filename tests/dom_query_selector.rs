use fastrender::dom::parse_html;
use fastrender::dom2::{Document, NodeKind};
use fastrender::web::dom::DomException;

fn node_id_attribute(doc: &Document, id: fastrender::dom2::NodeId, name: &str) -> Option<&str> {
  match &doc.node(id).kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case(name))
      .map(|(_, v)| v.as_str()),
    _ => None,
  }
}

#[test]
fn query_selector_id_returns_correct_node() {
  let dom = parse_html(r#"<!doctype html><div id="target"></div>"#).unwrap();
  let mut doc = Document::from_renderer_dom(&dom);

  let found = doc
    .query_selector("#target", None)
    .unwrap()
    .expect("missing match");
  assert_eq!(node_id_attribute(&doc, found, "id"), Some("target"));
}

#[test]
fn query_selector_all_returns_in_tree_order() {
  let dom = parse_html(
    r#"<!doctype html>
    <div>
      <span id="s1"></span>
      <span id="s2"></span>
    </div>
    <div>
      <span id="s3"></span>
    </div>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&dom);

  let ids: Vec<Option<&str>> = doc
    .query_selector_all("div span", None)
    .unwrap()
    .into_iter()
    .map(|node| node_id_attribute(&doc, node, "id"))
    .collect();

  assert_eq!(ids, vec![Some("s1"), Some("s2"), Some("s3")]);
}

#[test]
fn matches_selector_works() {
  let dom = parse_html(r#"<!doctype html><div id="x" class="cls other"></div>"#).unwrap();
  let mut doc = Document::from_renderer_dom(&dom);

  let node = doc.query_selector("#x", None).unwrap().expect("missing #x");
  assert!(doc.matches_selector(node, ".cls").unwrap());
  assert!(doc.matches_selector(node, "div.cls").unwrap());
  assert!(!doc.matches_selector(node, "span.cls").unwrap());
}

#[test]
fn element_scoped_query_scope_matches_self() {
  let dom = parse_html(r#"<!doctype html><div id="scope"><span></span></div>"#).unwrap();
  let mut doc = Document::from_renderer_dom(&dom);

  let scope = doc
    .query_selector("#scope", None)
    .unwrap()
    .expect("missing #scope");
  assert_eq!(
    doc.query_selector(":scope", Some(scope)).unwrap(),
    Some(scope)
  );
}

#[test]
fn invalid_selector_returns_syntax_error() {
  let dom = parse_html(r#"<!doctype html><div></div>"#).unwrap();
  let mut doc = Document::from_renderer_dom(&dom);

  let err = doc.query_selector("div[", None).unwrap_err();
  assert!(matches!(err, DomException::SyntaxError { .. }));
}

#[test]
fn query_selector_skips_inert_template_contents() {
  let dom = parse_html(
    r#"<!doctype html>
    <template><div id="inert"></div></template>
    <div id="live"></div>"#,
  )
  .unwrap();
  let mut doc = Document::from_renderer_dom(&dom);

  assert_eq!(doc.query_selector("#inert", None).unwrap(), None);
  assert_eq!(
    doc.query_selector("#live", None).unwrap().is_some(),
    true,
    "expected to find #live"
  );

  let ids: Vec<Option<&str>> = doc
    .query_selector_all("div", None)
    .unwrap()
    .into_iter()
    .map(|node| node_id_attribute(&doc, node, "id"))
    .collect();
  assert_eq!(ids, vec![Some("live")]);
}
