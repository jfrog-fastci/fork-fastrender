use crate::common::accessibility::{find_json_node, render_accessibility_json};

#[test]
fn accessibility_value_sanitizes_non_text_input_types_that_map_to_textbox() {
  let html = r#"
    <html><body>
      <input id="color" type="color" value="not-a-color">
      <input id="date" type="date" value="2024-99-99">
    </body></html>
  "#;

  let tree = render_accessibility_json(html);

  let color = find_json_node(&tree, "color").expect("color node");
  assert_eq!(color.get("role").and_then(|v| v.as_str()), Some("textbox"));
  assert_eq!(
    color.get("value").and_then(|v| v.as_str()),
    Some("#000000"),
    "expected invalid <input type=color> value to sanitize to #000000"
  );

  let date = find_json_node(&tree, "date").expect("date node");
  assert_eq!(date.get("role").and_then(|v| v.as_str()), Some("textbox"));
  let date_value = date.get("value").and_then(|v| v.as_str());
  assert!(
    date_value.is_none() || date_value == Some(""),
    "expected invalid <input type=date> value to sanitize to empty, got {date_value:?}"
  );
}
