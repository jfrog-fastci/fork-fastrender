#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::dom::DomNode;
use fastrender::ui::messages::{KeyAction, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Startup + first navigation can take a while under parallel test load.
const TIMEOUT: Duration = Duration::from_secs(20);
// Negative assertions should be bounded to avoid flakiness.
const NEGATIVE_WINDOW: Duration = Duration::from_millis(300);

fn node_id_by_html_id(dom: &DomNode, html_id: &str) -> usize {
  let ids = fastrender::dom::enumerate_dom_ids(dom);
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|id| id == html_id) {
      return *ids
        .get(&(node as *const DomNode))
        .unwrap_or_else(|| panic!("missing preorder id for element with id={html_id:?}"));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected element with id={html_id:?}");
}

#[test]
fn ui_datalist_ignores_datalist_inside_inert_template_for_input_list_resolution() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          input { position: absolute; left: 0; width: 200px; height: 30px; }
          #a { top: 0; }
          #b { top: 40px; }
        </style>
      </head>
      <body>
        <input id="a" list="dl" />
        <template>
          <datalist id="dl">
            <option value="trap"></option>
          </datalist>
        </template>

        <input id="b" list="dl2" />
        <datalist id="dl2">
          <option value="bee" label="Bee label"></option>
        </datalist>
      </body>
    </html>
  "#;

  let input_ids_dom = fastrender::dom::parse_html(html).expect("parse html");
  let input_a_id = node_id_by_html_id(&input_ids_dom, "a");
  let input_b_id = node_id_by_html_id(&input_ids_dom, "b");

  let url = site.write("page.html", html);

  let worker =
    fastrender::ui::spawn_browser_worker_with_factory(support::deterministic_factory())
      .expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(support::create_tab_msg(tab_id, Some(url)))
    .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, (260, 120), 1.0))
    .expect("ViewportChanged");

  match support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }
  while rx.try_recv().is_ok() {}

  let click = |pos_css: (f32, f32)| {
    tx.send(UiToWorker::PointerDown {
      tab_id,
      pos_css,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("PointerDown");
    tx.send(UiToWorker::PointerUp {
      tab_id,
      pos_css,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("PointerUp");
  };

  // Attempt to open datalist suggestions for #a. The only matching <datalist id=dl> lives inside an
  // inert <template>, so the worker must not emit DatalistOpened for this control.
  click((10.0, 10.0));
  tx.send(support::text_input(tab_id, "t"))
    .expect("TextInput(trap prefix)");
  tx.send(support::key_action(tab_id, KeyAction::ArrowDown))
    .expect("KeyAction(ArrowDown)");

  let msgs = support::drain_for(&rx, NEGATIVE_WINDOW);
  if msgs.iter().any(|msg| {
    matches!(
      msg,
      WorkerToUi::DatalistOpened {
        input_node_id, ..
      } if *input_node_id == input_a_id
    )
  }) {
    panic!(
      "unexpected DatalistOpened for #a (datalist is inside <template>):\n{}",
      support::format_messages(&msgs)
    );
  }

  while rx.try_recv().is_ok() {}

  // Now open datalist suggestions for #b. This datalist is in the normal DOM tree, so the worker
  // should emit DatalistOpened with the expected option.
  click((10.0, 50.0));
  tx.send(support::text_input(tab_id, "b"))
    .expect("TextInput(bee prefix)");
  tx.send(support::key_action(tab_id, KeyAction::ArrowDown))
    .expect("KeyAction(ArrowDown, b)");

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::DatalistOpened {
        input_node_id, ..
      } if *input_node_id == input_b_id
    )
  })
  .expect("expected DatalistOpened for #b");

  let WorkerToUi::DatalistOpened {
    input_node_id,
    options,
    ..
  } = msg
  else {
    unreachable!("filtered above");
  };
  assert_eq!(input_node_id, input_b_id);
  assert!(
    options.iter().any(|opt| opt.value == "bee"),
    "expected DatalistOpened to include the 'bee' suggestion, got options={options:?}"
  );

  drop(tx);
  drop(rx);
  join.join().unwrap();
}

