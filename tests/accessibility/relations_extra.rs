use fastrender::accessibility::AccessibilityNode;
use fastrender::api::FastRender;

fn find_by_id<'a>(node: &'a AccessibilityNode, id: &str) -> Option<&'a AccessibilityNode> {
  if node.id.as_deref() == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn aria_details_relation_resolves_idref() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <div id="src" aria-details="details">Source</div>
        <div id="details">More</div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let src = find_by_id(&tree, "src").expect("source node");
  let relations = src.relations.as_ref().expect("relations");
  assert_eq!(relations.details.as_deref(), Some("details"));
}

#[test]
fn aria_errormessage_relation_gated_by_invalid() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <input id="inv" required aria-errormessage="err" />
        <div id="err">Required</div>

        <input id="ok" value="x" aria-errormessage="err" />
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let inv = find_by_id(&tree, "inv").expect("invalid input");
  assert!(inv.states.invalid);
  let relations = inv.relations.as_ref().expect("relations");
  assert_eq!(relations.error_message.as_deref(), Some("err"));

  let ok = find_by_id(&tree, "ok").expect("valid input");
  assert!(!ok.states.invalid);
  assert_eq!(
    ok.relations
      .as_ref()
      .and_then(|relations| relations.error_message.as_deref()),
    None,
    "valid controls should not expose aria-errormessage relationship"
  );
}

#[test]
fn aria_details_idref_respects_shadow_scopes() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <div id="outside">Outside</div>
        <div id="host">
          <template shadowroot="open">
            <div id="inside">Inside</div>
            <div id="src-inside" aria-details="inside">Source inside</div>
            <div id="src-outside" aria-details="outside">Source outside</div>
          </template>
        </div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let src_inside = find_by_id(&tree, "src-inside").expect("shadow source inside");
  let relations = src_inside.relations.as_ref().expect("relations");
  assert_eq!(relations.details.as_deref(), Some("inside"));

  let src_outside = find_by_id(&tree, "src-outside").expect("shadow source outside");
  assert_eq!(
    src_outside
      .relations
      .as_ref()
      .and_then(|relations| relations.details.as_deref()),
    None,
    "shadow root nodes should not resolve IDREFs to document-scoped IDs"
  );
}

#[test]
fn aria_labelledby_and_describedby_relations_resolve_and_dedupe() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <div id="l1">Label 1</div>
        <div id="l2">Label 2</div>
        <div id="d1">Desc 1</div>
        <div id="d2">Desc 2</div>
        <div
          id="target"
          tabindex="0"
          aria-labelledby="l1 l1 l2"
          aria-describedby="d1 d2"
        ></div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let target = find_by_id(&tree, "target").expect("target node");
  let relations = target.relations.as_ref().expect("relations");
  assert_eq!(relations.labelled_by, vec!["l1".to_string(), "l2".to_string()]);
  assert_eq!(relations.described_by, vec!["d1".to_string(), "d2".to_string()]);
}

#[test]
fn aria_labelledby_and_describedby_relations_ignore_unresolved_ids() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <div id="target" tabindex="0" aria-labelledby="missing" aria-describedby="missing2"></div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let target = find_by_id(&tree, "target").expect("target node");
  assert!(
    target.relations.is_none(),
    "unresolved IDREFs should not be exported as relationships"
  );
}

#[test]
fn aria_labelledby_relation_respects_shadow_scopes() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <div id="outside">Outside</div>
        <div id="host">
          <template shadowroot="open">
            <div id="inside">Inside</div>
            <div id="src-inside" tabindex="0" aria-labelledby="inside"></div>
            <div id="src-outside" tabindex="0" aria-labelledby="outside"></div>
          </template>
        </div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  let src_inside = find_by_id(&tree, "src-inside").expect("shadow source inside");
  let relations = src_inside.relations.as_ref().expect("relations");
  assert_eq!(relations.labelled_by, vec!["inside".to_string()]);

  let src_outside = find_by_id(&tree, "src-outside").expect("shadow source outside");
  assert!(
    src_outside.relations.is_none(),
    "shadow root nodes should not resolve IDREF lists to document-scoped IDs"
  );
}
