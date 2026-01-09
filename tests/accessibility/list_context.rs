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
fn presentational_list_suppresses_listitem_role() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <ul id="l" role="presentation">
          <li id="i">Item</li>
        </ul>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  assert!(
    find_by_id(&tree, "l").is_none(),
    "presentational list container should be omitted"
  );

  let item = find_by_id(&tree, "i").expect("list item");
  assert_eq!(item.role, "generic");
}

#[test]
fn presentational_definition_list_suppresses_term_and_definition_roles() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <dl id="d" role="presentation">
          <dt id="t">Term</dt>
          <dd id="def">Def</dd>
        </dl>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree");

  assert!(
    find_by_id(&tree, "d").is_none(),
    "presentational definition list container should be omitted"
  );

  let term = find_by_id(&tree, "t").expect("term node");
  assert_eq!(term.role, "generic");

  let def = find_by_id(&tree, "def").expect("definition node");
  assert_eq!(def.role, "generic");
}
