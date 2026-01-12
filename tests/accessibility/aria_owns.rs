use crate::common::accessibility::{count_by_id, find_by_id, find_path, render_accessibility_tree};

#[test]
fn aria_owns_reparents_out_of_subtree_node() {
  let html = r#"
    <html>
      <body>
        <div id="owner" aria-owns="target">
          <span>Inside</span>
        </div>
        <button id="target">Target</button>
      </body>
    </html>
  "#;

  let tree = render_accessibility_tree(html);
  let owner = find_by_id(&tree, "owner").expect("owner node");
  assert!(
    owner
      .children
      .iter()
      .any(|child| child.id.as_deref() == Some("target")),
    "owned target should appear under its owner"
  );

  assert_eq!(
    count_by_id(&tree, "target"),
    1,
    "owned target should appear exactly once"
  );

  let path = find_path(&tree, "target").expect("path to target");
  assert_eq!(
    path[path.len() - 2].id.as_deref(),
    Some("owner"),
    "owned target's parent should be the owner node"
  );
}

#[test]
fn aria_owns_cycle_is_ignored() {
  let html = r#"
    <html>
      <body>
        <div id="a" aria-label="A" aria-owns="b"></div>
        <div id="b" aria-label="B" aria-owns="a"></div>
      </body>
    </html>
  "#;

  let tree = render_accessibility_tree(html);
  assert_eq!(count_by_id(&tree, "a"), 1);
  assert_eq!(count_by_id(&tree, "b"), 1);

  let path_b = find_path(&tree, "b").expect("path to b");
  assert_eq!(
    path_b[path_b.len() - 2].id.as_deref(),
    Some("a"),
    "b should be owned by a (first owner wins)"
  );

  let path_a = find_path(&tree, "a").expect("path to a");
  assert!(
    path_a.iter().all(|node| node.id.as_deref() != Some("b")),
    "ownership cycle must not reparent a under b"
  );
}

#[test]
fn aria_owns_ignores_hidden_targets() {
  let html = r#"
    <html>
      <body>
        <div id="owner" aria-owns="hidden inert csshidden"></div>
        <div id="hidden" aria-hidden="true" aria-label="Hidden"></div>
        <div id="inert" inert aria-label="Inert"></div>
        <div id="csshidden" style="display:none" aria-label="CSS Hidden"></div>
      </body>
    </html>
  "#;

  let tree = render_accessibility_tree(html);
  assert!(
    find_by_id(&tree, "owner").is_none(),
    "owner should not be forced into the tree when all owned targets are hidden"
  );
  assert_eq!(count_by_id(&tree, "hidden"), 0);
  assert_eq!(count_by_id(&tree, "inert"), 0);
  assert_eq!(count_by_id(&tree, "csshidden"), 0);
}
