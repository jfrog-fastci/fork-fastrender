use crate::common::accessibility::{find_json_node, render_accessibility_json};

#[test]
fn aria_has_popup_validates_tokens_and_ignores_empty() {
  let html = r##"
    <html>
      <body>
        <button id="bogus" aria-haspopup="bogus">Bogus</button>
        <button id="empty" aria-haspopup>Empty</button>
        <button id="false" aria-haspopup="false">False</button>
        <button id="zero" aria-haspopup="0">Zero</button>
        <button id="true" aria-haspopup="true">True</button>
        <button id="one" aria-haspopup="1">One</button>
        <button id="menu" aria-haspopup="menu">Menu</button>
        <button id="dialog" aria-haspopup="dialog">Dialog</button>
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
  assert!(
    empty_states.get("has_popup").is_none(),
    "empty/minimized aria-haspopup should be treated as not specified"
  );

  for id in ["false", "zero"] {
    let node = find_json_node(&tree, id).unwrap();
    let states = node.get("states").unwrap();
    assert!(
      states.get("has_popup").is_none(),
      "aria-haspopup={id:?} should not set has_popup"
    );
  }

  for (id, expected) in [
    ("true", "true"),
    ("one", "true"),
    ("menu", "menu"),
    ("dialog", "dialog"),
  ] {
    let node = find_json_node(&tree, id).unwrap();
    let states = node.get("states").unwrap();
    assert_eq!(
      states.get("has_popup").and_then(|v| v.as_str()),
      Some(expected),
      "aria-haspopup={id:?} should serialize {expected:?}"
    );
  }

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
        <div id="false" role="textbox" aria-label="False" aria-multiline="false" tabindex="0"></div>
        <div id="zero" role="textbox" aria-label="Zero" aria-multiline="0" tabindex="0"></div>
        <div id="true" role="textbox" aria-label="True" aria-multiline="true" tabindex="0"></div>
        <div id="one" role="textbox" aria-label="One" aria-multiline="1" tabindex="0"></div>
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

  for (id, expected) in [
    ("false", false),
    ("zero", false),
    ("true", true),
    ("one", true),
  ] {
    let node = find_json_node(&tree, id).unwrap();
    let states = node.get("states").unwrap();
    assert_eq!(
      states.get("multiline").and_then(|v| v.as_bool()),
      Some(expected),
      "aria-multiline={id:?} should serialize {expected}"
    );
  }
}

#[test]
fn aria_live_treats_value_as_single_token() {
  let html = r##"
    <html>
      <body>
        <div id="empty" role="status" aria-live>Empty</div>
        <div id="bogus" role="status" aria-live="bogus">Bogus</div>
        <div id="multi" role="status" aria-live="polite assertive">Multi</div>
        <div id="polite" role="status" aria-live="polite">Polite</div>
        <div id="assertive" role="status" aria-live="ASSERTIVE">Assertive</div>
      </body>
    </html>
  "##;

  let tree = render_accessibility_json(html);

  for id in ["empty", "bogus", "multi"] {
    let node = find_json_node(&tree, id).unwrap();
    let states = node.get("states").unwrap();
    assert!(
      states.get("live").is_none(),
      "aria-live={id:?} should not set live"
    );
  }

  for (id, expected) in [("polite", "polite"), ("assertive", "assertive")] {
    let node = find_json_node(&tree, id).unwrap();
    let states = node.get("states").unwrap();
    assert_eq!(
      states.get("live").and_then(|v| v.as_str()),
      Some(expected),
      "aria-live={id:?} should serialize {expected:?}"
    );
  }
}
