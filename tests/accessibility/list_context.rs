use crate::common::accessibility::{find_by_id, render_accessibility_tree};

#[test]
fn presentational_list_suppresses_listitem_role() {
  let html = r##"
    <html>
      <body>
        <ul id="l" role="presentation">
          <li id="i">Item</li>
        </ul>
      </body>
    </html>
  "##;
  let tree = render_accessibility_tree(html);

  assert!(
    find_by_id(&tree, "l").is_none(),
    "presentational list container should be omitted"
  );

  let item = find_by_id(&tree, "i").expect("list item");
  assert_eq!(item.role, "generic");
}

#[test]
fn presentational_definition_list_suppresses_term_and_definition_roles() {
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
  let tree = render_accessibility_tree(html);

  assert!(
    find_by_id(&tree, "d").is_none(),
    "presentational definition list container should be omitted"
  );

  let term = find_by_id(&tree, "t").expect("term node");
  assert_eq!(term.role, "generic");

  let def = find_by_id(&tree, "def").expect("definition node");
  assert_eq!(def.role, "generic");
}
