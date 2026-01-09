#![cfg(feature = "browser_ui")]

use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::tree::box_tree::SelectItem;
use fastrender::ui::messages::{PointerButton, RepaintReason, UiToWorker, WorkerToUi};
use fastrender::ui::{BrowserTabController, TabId};

fn node_id_by_id_attr(root: &DomNode, id_attr: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
      return *ids
        .get(&(node as *const DomNode))
        .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("no element with id attribute {id_attr:?}");
}

#[test]
fn select_dropdown_open_and_choose_roundtrip() {
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let dpr = 1.0;
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          select { position: absolute; left: 0; top: 0; width: 200px; height: 28px; }
        </style>
      </head>
      <body>
        <select id="sel">
          <option id="opt_disabled" disabled>Disabled</option>
          <optgroup label="Group A">
            <option id="opt_one" value="one">One</option>
            <option id="opt_two" value="two" selected>Two</option>
          </optgroup>
          <optgroup label="Group B" disabled>
            <option id="opt_three" value="three">Three</option>
          </optgroup>
          <option id="opt_four" value="four">Four</option>
        </select>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html(
    tab_id,
    html,
    "https://example.com/",
    viewport_css,
    dpr,
  )
  .expect("controller from_html");

  // Ensure a prepared tree exists for hit-testing and geometry queries.
  let _ = controller
    .handle_message(UiToWorker::RequestRepaint {
      tab_id,
      reason: RepaintReason::Explicit,
    })
    .expect("request repaint");

  let select_node_id = node_id_by_id_attr(controller.document().dom(), "sel");
  let option_one_id = node_id_by_id_attr(controller.document().dom(), "opt_one");

  // Click inside the select control.
  let _ = controller
    .handle_message(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer down");

  let out = controller
    .handle_message(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer up");

  let opened = out.into_iter().find_map(|msg| match msg {
    WorkerToUi::SelectDropdownOpened {
      tab_id: got_tab,
      select_node_id: got_select,
      control,
      anchor_css,
    } if got_tab == tab_id => Some((got_select, control, anchor_css)),
    _ => None,
  });

  let (got_select_id, control, anchor_css) =
    opened.expect("expected SelectDropdownOpened message");
  assert_eq!(got_select_id, select_node_id);

  let labels: Vec<(String, bool)> = control
    .items
    .iter()
    .map(|item| match item {
      SelectItem::OptGroupLabel { label, disabled } => (format!("optgroup:{label}"), *disabled),
      SelectItem::Option { label, disabled, .. } => (format!("option:{label}"), *disabled),
    })
    .collect();

  assert_eq!(
    labels,
    vec![
      ("option:Disabled".to_string(), true),
      ("optgroup:Group A".to_string(), false),
      ("option:One".to_string(), false),
      ("option:Two".to_string(), false),
      ("optgroup:Group B".to_string(), true),
      ("option:Three".to_string(), true),
      ("option:Four".to_string(), false),
    ]
  );

  assert!(
    anchor_css.x().is_finite()
      && anchor_css.y().is_finite()
      && anchor_css.width().is_finite()
      && anchor_css.height().is_finite(),
    "anchor_css must be finite (got {anchor_css:?})"
  );
  assert!(
    anchor_css.min_x() >= 0.0
      && anchor_css.min_y() >= 0.0
      && anchor_css.max_x() <= viewport_css.0 as f32
      && anchor_css.max_y() <= viewport_css.1 as f32,
    "anchor_css should be within viewport (viewport={viewport_css:?}, anchor={anchor_css:?})"
  );

  // Choose an enabled option.
  let _ = controller
    .handle_message(UiToWorker::select_dropdown_choose(
      tab_id,
      select_node_id,
      option_one_id,
    ))
    .expect("select dropdown choose");

  let dom = controller.document().dom();
  let ids = enumerate_dom_ids(dom);

  let mut select_ptr: Option<*const DomNode> = None;
  let mut option_one_ptr: Option<*const DomNode> = None;
  let mut stack: Vec<&DomNode> = vec![dom];
  while let Some(node) = stack.pop() {
    if ids.get(&(node as *const DomNode)).copied() == Some(select_node_id) {
      select_ptr = Some(node as *const DomNode);
    }
    if ids.get(&(node as *const DomNode)).copied() == Some(option_one_id) {
      option_one_ptr = Some(node as *const DomNode);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let select_node = unsafe { &*select_ptr.expect("select node pointer") };
  assert_eq!(
    select_node.get_attribute_ref("data-fastr-user-validity"),
    Some("true"),
    "expected select to be marked user-valid after choosing an option"
  );

  let option_one_node = unsafe { &*option_one_ptr.expect("option node pointer") };
  assert!(
    option_one_node.get_attribute_ref("selected").is_some(),
    "expected chosen option to have selected attribute"
  );
}
