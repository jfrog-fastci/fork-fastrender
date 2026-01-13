#![cfg(feature = "a11y_accesskit")]

use accesskit::Action;
use fastrender::renderer_chrome::accesskit_actions::route_accesskit_action_to_dom;
use fastrender::ui::PointerModifiers;
use fastrender::{BrowserTab, Rect, RenderOptions, Result, VmJsBrowserTabExecutor};

#[test]
fn show_context_menu_dispatches_trusted_contextmenu_event_and_honors_prevent_default() -> Result<()> {
  let html = r#"<!doctype html>
<div id="target"></div>
<script>
  const t = document.getElementById("target");
  t.addEventListener("contextmenu", (e) => {
    t.setAttribute("data-seen", e.type);
    t.setAttribute("data-trusted", String(e.isTrusted));
    t.setAttribute("data-client-x", String(e.clientX));
    t.setAttribute("data-client-y", String(e.clientY));
    t.setAttribute("data-button", String(e.button));
    t.setAttribute("data-buttons", String(e.buttons));
    e.preventDefault();
  });
</script>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;
  let target = tab
    .dom()
    .get_element_by_id("target")
    .expect("expected #target element");

  let bounds = Some(Rect::from_xywh(10.0, 20.0, 30.0, 40.0));
  let outcome = route_accesskit_action_to_dom(
    &mut tab,
    Action::ShowContextMenu,
    target,
    bounds,
    PointerModifiers::NONE,
  )?;
  assert_eq!(
    outcome,
    Some(false),
    "expected JS preventDefault() to block the default context menu action"
  );

  assert_eq!(
    tab.dom().get_attribute(target, "data-seen").unwrap(),
    Some("contextmenu")
  );
  assert_eq!(
    tab.dom().get_attribute(target, "data-trusted").unwrap(),
    Some("true"),
    "expected synthesized contextmenu event to be trusted"
  );
  assert_eq!(
    tab.dom().get_attribute(target, "data-client-x").unwrap(),
    Some("25"),
    "expected clientX to be the center of the provided bounds"
  );
  assert_eq!(
    tab.dom().get_attribute(target, "data-client-y").unwrap(),
    Some("40"),
    "expected clientY to be the center of the provided bounds"
  );
  assert_eq!(tab.dom().get_attribute(target, "data-button").unwrap(), Some("2"));
  assert_eq!(
    tab.dom().get_attribute(target, "data-buttons").unwrap(),
    Some("2")
  );

  Ok(())
}
