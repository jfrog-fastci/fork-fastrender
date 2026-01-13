use crate::common::accessibility::{find_json_node, render_accessibility_tree};

#[test]
fn accessibility_node_id_is_not_serialized_to_json() {
  let html = r##"
    <html>
      <body>
        <button id="btn">Press</button>
      </body>
    </html>
  "##;

  let tree = render_accessibility_tree(html);
  let json = serde_json::to_value(&tree).expect("serialize accessibility tree");

  assert!(
    json.get("node_id").is_none(),
    "root document node should not serialize the internal node_id field"
  );

  let btn = find_json_node(&json, "btn").expect("button node");
  assert!(
    btn.get("node_id").is_none(),
    "descendant nodes should not serialize the internal node_id field"
  );
}

