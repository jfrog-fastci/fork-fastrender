#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::test_worker::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  support::rgba_at(&frame.pixmap, x_px, y_px)
}

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady {
      tab_id: msg_tab,
      frame,
    } => {
      assert_eq!(msg_tab, tab_id);
      frame
    }
    WorkerToUi::NavigationFailed {
      tab_id: msg_tab,
      url,
      error,
      ..
    } => {
      assert_eq!(msg_tab, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn wait_for_rgb_at_css(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  css_pos: (u32, u32),
  expected: (u8, u8, u8),
  deadline: Instant,
) -> RenderedFrame {
  loop {
    if Instant::now() >= deadline {
      panic!(
        "timed out waiting for pixel at {:?} to become {:?}",
        css_pos, expected
      );
    }
    let frame = next_frame_ready(rx, tab_id);
    let rgba = rgba_at_css(&frame, css_pos.0, css_pos.1);
    if rgba == [expected.0, expected.1, expected.2, 255] {
      return frame;
    }
  }
}

#[test]
fn dropdown_select_pick_updates_dom_and_repaints() {
  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #sel { position: absolute; left: 0; top: 0; width: 120px; height: 24px; }
      #box { position: absolute; left: 0; top: 40px; width: 64px; height: 64px; background: rgb(255,0,0); }
      select:has(option[selected][value="b"]) + #box { background: rgb(0,255,0); }
    </style>
  </head>
  <body>
    <select id="sel">
      <option value="a" selected>Red</option>
      <optgroup label="Group">
        <option value="b">Green</option>
      </optgroup>
    </select>
    <div id="box"></div>
  </body>
</html>
"#,
  );

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-select-dropdown-pick")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: CancelGens::new(),
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (160, 160),
      dpr: 1.0,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  let frame = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(rgba_at_css(&frame, 10, 50), [255, 0, 0, 255]);

  // Click the <select> to open the dropdown.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();

  let opened = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::SelectDropdownOpened { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for SelectDropdownOpened for tab {tab_id:?}"));

  let (select_node_id, control) = match opened {
    WorkerToUi::SelectDropdownOpened {
      tab_id: msg_tab,
      select_node_id,
      control,
      ..
    } => {
      assert_eq!(msg_tab, tab_id);
      (select_node_id, control)
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  };

  assert!(
    control.items.len() >= 3,
    "expected select control to contain an optgroup label and an option (got {:?})",
    control
  );
  assert!(
    control
      .items
      .get(1)
      .is_some_and(|item| item.is_optgroup_label() && item.label() == "Group"),
    "expected item_index=1 to be an optgroup label row (got {:?})",
    control.items.get(1)
  );

  let item_index = control
    .items
    .iter()
    .position(|item| item.is_option() && item.label() == "Green")
    .expect("expected select dropdown to contain option label 'Green'");
  assert_eq!(
    item_index, 2,
    "expected optgroup label row to be included in flattened item order"
  );

  ui_tx
    .send(UiToWorker::SelectDropdownPick {
      tab_id,
      select_node_id,
      item_index,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = wait_for_rgb_at_css(&ui_rx, tab_id, (10, 50), (0, 255, 0), deadline);
  assert_eq!(rgba_at_css(&frame, 10, 50), [0, 255, 0, 255]);

  drop(ui_tx);
  join.join().unwrap();
}
