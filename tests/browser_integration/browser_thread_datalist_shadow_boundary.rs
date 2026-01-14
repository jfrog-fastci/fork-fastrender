#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn open_datalist(
  tx: &std::sync::mpsc::Sender<UiToWorker>,
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
  tab_id: TabId,
  click_pos: (f32, f32),
  text: &str,
) -> Vec<String> {
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })
  .expect("PointerDown");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })
  .expect("PointerUp");

  tx.send(UiToWorker::TextInput {
    tab_id,
    text: text.to_string(),
  })
  .expect("TextInput");

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::DatalistOpened { .. })
  })
  .unwrap_or_else(|| {
    // Best-effort debug dump for easier diagnosis on failure.
    let tail = support::drain_for(rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for DatalistOpened. recent messages:\n{}",
      support::format_messages(&tail)
    );
  });

  let WorkerToUi::DatalistOpened { options, .. } = msg else {
    unreachable!("filtered above");
  };
  options.into_iter().map(|opt| opt.value).collect()
}

#[test]
fn browser_thread_datalist_list_attr_respects_shadow_root_id_boundary() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #host { position: absolute; left: 0; top: 0; width: 240px; height: 30px; }
          #out { position: absolute; left: 0; top: 60px; width: 240px; height: 30px; }
        </style>
      </head>
      <body>
        <div id="host">
          <template shadowroot="open">
            <style>
              #in { width: 240px; height: 30px; }
            </style>
            <input id="in" list="dl">
            <datalist id="dl">
              <option value="shadow-one"></option>
            </datalist>
          </template>
        </div>

        <input id="out" list="dl">
        <datalist id="dl">
          <option value="light-one"></option>
        </datalist>
      </body>
    </html>
  "#;
  let url = site.write("page.html", html);

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(support::create_tab_msg_with_cancel(
    tab_id,
    Some(url),
    CancelGens::new(),
  ))
  .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, (320, 160), 1.0))
    .expect("ViewportChanged");

  // Wait for the first rendered frame so the tab has a live document.
  match support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }

  // Drain initial messages.
  while rx.try_recv().is_ok() {}

  // Open datalist for the shadow-root input. The suggestions must come from the shadow `<datalist>`.
  let shadow_values = open_datalist(&tx, &rx, tab_id, (10.0, 10.0), "s");
  assert_eq!(
    shadow_values,
    vec!["shadow-one".to_string()],
    "expected #in to resolve list=dl inside the shadow root"
  );

  // Drain any follow-up messages (e.g. close notifications) before interacting with the second input.
  while rx.try_recv().is_ok() {}

  // Open datalist for the light-DOM input. The suggestions must come from the light `<datalist>`.
  let light_values = open_datalist(&tx, &rx, tab_id, (10.0, 70.0), "l");
  assert_eq!(
    light_values,
    vec!["light-one".to_string()],
    "expected #out to resolve list=dl in the document tree"
  );

  drop(tx);
  drop(rx);
  join.join().unwrap();
}
