#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn recv_frame(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> RenderedFrame {
  let start = Instant::now();
  let mut seen = Vec::new();
  loop {
    let remaining = timeout.saturating_sub(start.elapsed());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for FrameReady\n{}",
      support::format_messages(&seen)
    );
    let msg = support::recv_for_tab(
      rx,
      tab_id,
      remaining.min(Duration::from_millis(200)),
      |_| true,
    );
    let Some(msg) = msg else {
      continue;
    };
    match msg {
      WorkerToUi::FrameReady { frame, .. } => return frame,
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!(
          "navigation failed while waiting for FrameReady ({url}): {error}\n{}",
          support::format_messages(&seen)
        );
      }
      other => seen.push(other),
    }
  }
}

fn wait_for_marker_color(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  expected_rgba: [u8; 4],
  timeout: Duration,
) {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for marker color {expected_rgba:?}"
    );

    let msg = support::recv_for_tab(
      rx,
      tab_id,
      remaining.min(Duration::from_millis(200)),
      |msg| matches!(msg, WorkerToUi::FrameReady { .. }),
    );
    let Some(WorkerToUi::FrameReady { frame, .. }) = msg else {
      continue;
    };

    if support::rgba_at(&frame.pixmap, 10, 100) == expected_rgba {
      break;
    }
  }
}

#[test]
fn select_listbox_multiple_click_modifiers_match_browsers() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          /* Deterministic row height for hit-testing. */
          #s { display: block; width: 160px; height: 80px; padding: 0; border: 0; font-size: 16px; line-height: 20px; }
          #marker { width: 64px; height: 64px; background: rgb(0,0,0); }

          /* Encode the select's selectedness set into the marker color (assert via pixels). */
          #s:has(option#o1[selected]):not(:has(option[selected]:not(#o1))) + #marker { background: rgb(255,0,0); }
          #s:has(option#o2[selected]):not(:has(option[selected]:not(#o2))) + #marker { background: rgb(0,255,0); }
          #s:has(option#o2[selected]):has(option#o4[selected]):not(:has(option[selected]:not(#o2):not(#o4))) + #marker { background: rgb(0,0,255); }
          #s:has(option#o1[selected]):has(option#o2[selected]):has(option#o3[selected]):has(option#o4[selected]) + #marker { background: rgb(255,255,0); }
        </style>
      </head>
      <body>
        <select id="s" multiple size="4">
          <option id="o1" selected>One</option>
          <option id="o2">Two</option>
          <option id="o3">Three</option>
          <option id="o4">Four</option>
        </select>
        <div id="marker"></div>
      </body>
    </html>
  "#;
  let url = site.write("page.html", html);

  let worker =
    spawn_ui_worker("fastr-ui-worker-select-listbox-multiple-selection").expect("spawn ui worker");
  let tab_id = TabId::new();
  worker
    .ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("CreateTab");
  worker
    .ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 200),
      dpr: 1.0,
    })
    .expect("ViewportChanged");
  worker
    .ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate");

  let frame = recv_frame(&worker.ui_rx, tab_id, TIMEOUT);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 100),
    [255, 0, 0, 255],
    "expected initial selection (only option 1) to be encoded as red"
  );

  // 1) Plain click option 2 → replacement selection (only option 2 selected).
  let opt2_click = (10.0_f32, 30.0_f32);
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: opt2_click,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("PointerDown opt2");
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: opt2_click,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("PointerUp opt2");
  wait_for_marker_color(&worker.ui_rx, tab_id, [0, 255, 0, 255], TIMEOUT);

  // 2) Ctrl/Cmd click option 4 → toggles (options 2 and 4 selected).
  let command_mod = if cfg!(target_os = "macos") {
    PointerModifiers::META
  } else {
    PointerModifiers::CTRL
  };
  let opt4_click = (10.0_f32, 70.0_f32);
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: opt4_click,
      button: PointerButton::Primary,
      modifiers: command_mod,
      click_count: 1,
    })
    .expect("PointerDown opt4 ctrl");
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: opt4_click,
      button: PointerButton::Primary,
      modifiers: command_mod,
    })
    .expect("PointerUp opt4 ctrl");
  wait_for_marker_color(&worker.ui_rx, tab_id, [0, 0, 255, 255], TIMEOUT);

  // 3) Shift click option 1 → range-select from the current anchor (option 4) to option 1.
  //
  // Browsers treat the most recent non-shift interaction as the range-selection anchor, even when
  // that interaction was a Ctrl/Cmd-toggle click.
  let opt1_click = (10.0_f32, 10.0_f32);
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: opt1_click,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
      click_count: 1,
    })
    .expect("PointerDown opt1 shift");
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: opt1_click,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
    })
    .expect("PointerUp opt1 shift");
  wait_for_marker_color(&worker.ui_rx, tab_id, [255, 255, 0, 255], TIMEOUT);

  worker.join().expect("join worker");
}
