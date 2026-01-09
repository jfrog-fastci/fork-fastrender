mod js_harness;

use fastrender::dom::{DomNode, DomNodeType};
use fastrender::js::RunLimits;
use fastrender::Result;
use js_harness::Harness;

fn find_element_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  let mut stack = vec![node];
  while let Some(cur) = stack.pop() {
    if matches!(
      &cur.node_type,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. }
    ) {
      if cur.get_attribute_ref("id") == Some(id) {
        return Some(cur);
      }
    }
    for child in cur.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn harness_smoke_exec_timer_advance_and_dom_mutation() -> Result<()> {
  let html = "<!doctype html><html><body><div id='root'></div></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      setTimeout(() => {
        document.getElementById("root").setAttribute("data-done", "1");
      }, 10);
    "#,
  )?;

  // Timer isn't due yet.
  h.run_until_idle(RunLimits::unbounded())?;
  let dom = h.snapshot_dom();
  let root = find_element_by_id(&dom, "root").expect("expected #root to exist");
  assert!(root.get_attribute_ref("data-done").is_none());

  // Advance virtual time and run again.
  h.advance_time(10);
  h.run_until_idle(RunLimits::unbounded())?;

  let dom = h.snapshot_dom();
  let root = find_element_by_id(&dom, "root").expect("expected #root to exist");
  assert_eq!(root.get_attribute_ref("data-done"), Some("1"));
  Ok(())
}
