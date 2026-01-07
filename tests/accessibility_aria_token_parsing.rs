use fastrender::api::FastRender;
use serde_json::Value;

fn render_accessibility_json(html: &str) -> Value {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let json = renderer
    .accessibility_tree_json(&dom, 800, 600)
    .expect("accessibility tree json");
  serde_json::from_str(&json).expect("parse json")
}

fn find_json_node<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
  if node
    .get("id")
    .and_then(|v| v.as_str())
    .is_some_and(|v| v == id)
  {
    return Some(node);
  }

  if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
    for child in children {
      if let Some(found) = find_json_node(child, id) {
        return Some(found);
      }
    }
  }

  None
}

#[test]
fn aria_has_popup_validates_tokens_and_handles_empty() {
  let html = r##"
    <html>
      <body>
        <button id="bogus" aria-haspopup="bogus">Bogus</button>
        <button id="empty" aria-haspopup>Empty</button>
        <button id="menu" aria-haspopup="menu">Menu</button>
      </body>
    </html>
  "##;

  let tree = render_accessibility_json(html);

  let bogus = find_json_node(&tree, "bogus").expect("bogus button");
  let bogus_states = bogus.get("states").expect("states");
  assert!(
    bogus_states.get("has_popup").is_none(),
    "invalid aria-haspopup tokens should be ignored"
  );

  let empty = find_json_node(&tree, "empty").expect("empty button");
  let empty_states = empty.get("states").expect("states");
  assert_eq!(
    empty_states.get("has_popup").and_then(|v| v.as_str()),
    Some("true"),
    "minimized aria-haspopup should default to true"
  );

  let menu = find_json_node(&tree, "menu").expect("menu button");
  let menu_states = menu.get("states").expect("states");
  assert_eq!(
    menu_states.get("has_popup").and_then(|v| v.as_str()),
    Some("menu")
  );
}

#[test]
fn aria_multiline_ignores_empty_and_invalid_tokens() {
  let html = r##"
    <html>
      <body>
        <div id="empty" role="textbox" aria-label="Empty" aria-multiline="" tabindex="0"></div>
        <div id="bogus" role="textbox" aria-label="Bogus" aria-multiline="bogus" tabindex="0"></div>
        <div id="true" role="textbox" aria-label="True" aria-multiline="true" tabindex="0"></div>
      </body>
    </html>
  "##;

  let tree = render_accessibility_json(html);

  for id in ["empty", "bogus"] {
    let node = find_json_node(&tree, id).unwrap();
    let states = node.get("states").unwrap();
    assert!(
      states.get("multiline").is_none(),
      "aria-multiline={id:?} should not set multiline"
    );
  }

  let node = find_json_node(&tree, "true").expect("true textbox");
  let states = node.get("states").expect("states");
  assert_eq!(states.get("multiline").and_then(|v| v.as_bool()), Some(true));
}
