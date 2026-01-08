use fastrender::accessibility::AccessibilityNode;
use fastrender::api::FastRender;

fn render_accessibility_tree(html: &str) -> AccessibilityNode {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");
  renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree")
}

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
fn aria_controls_owns_and_activedescendant_relations() {
  let html = r#"
    <html>
      <body>
        <div id="panel">Panel</div>
        <div id="list" role="listbox">
          <div id="opt1" role="option">One</div>
          <div id="opt2" role="option">Two</div>
        </div>

        <button id="controller" aria-controls="list panel panel missing">Controller</button>

        <div
          id="combo"
          role="combobox"
          tabindex="0"
          aria-label="Combo"
          aria-controls="list"
          aria-activedescendant="opt2"
        ></div>

        <div
          id="combo-bad"
          role="combobox"
          tabindex="0"
          aria-label="Combo bad"
          aria-activedescendant="opt1 opt2"
        ></div>

        <div
          id="owner"
          role="listbox"
          tabindex="0"
          aria-label="Owner"
          aria-owns="panel list missing"
        ></div>
      </body>
    </html>
  "#;

  let tree = render_accessibility_tree(html);

  let controller = find_by_id(&tree, "controller").expect("controller node");
  let controller_relations = controller.relations.as_ref().expect("controller relations");
  assert_eq!(controller_relations.controls, vec!["list", "panel"]);

  let combo = find_by_id(&tree, "combo").expect("combo node");
  let combo_relations = combo.relations.as_ref().expect("combo relations");
  assert_eq!(combo_relations.controls, vec!["list"]);
  assert_eq!(combo_relations.active_descendant.as_deref(), Some("opt2"));

  let combo_bad = find_by_id(&tree, "combo-bad").expect("combo-bad node");
  assert!(
    combo_bad
      .relations
      .as_ref()
      .and_then(|r| r.active_descendant.as_deref())
      .is_none(),
    "aria-activedescendant is an IDREF, not a list"
  );

  let owner = find_by_id(&tree, "owner").expect("owner node");
  let owner_relations = owner.relations.as_ref().expect("owner relations");
  assert_eq!(owner_relations.owns, vec!["panel", "list"]);
}

#[test]
fn html_label_association_for_and_wrapping() {
  let html = r#"
    <html>
      <body>
        <label for="for-input">For label</label>
        <input id="for-input" type="text" />

        <label>
          Wrap label
          <input id="wrap-input" type="text" />
        </label>

        <label>
          First target
          <input id="first-target" type="text" />
          <span>More text</span>
          <input id="second-target" type="text" />
        </label>
      </body>
    </html>
  "#;

  let tree = render_accessibility_tree(html);

  let for_input = find_by_id(&tree, "for-input").expect("for-input");
  assert_eq!(for_input.name.as_deref(), Some("For label"));

  let wrap_input = find_by_id(&tree, "wrap-input").expect("wrap-input");
  assert_eq!(wrap_input.name.as_deref(), Some("Wrap label"));

  let first_target = find_by_id(&tree, "first-target").expect("first-target");
  assert_eq!(first_target.name.as_deref(), Some("First target More text"));

  let second_target = find_by_id(&tree, "second-target").expect("second-target");
  assert_eq!(second_target.name, None);
}

