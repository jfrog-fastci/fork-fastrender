#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{RenderedFrame, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use fastrender::RenderOptions;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn render_expected(viewport_css: (u32, u32), html: &str) -> tiny_skia::Pixmap {
  let mut renderer = support::deterministic_renderer();
  renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(viewport_css.0, viewport_css.1)
        .with_device_pixel_ratio(1.0),
    )
    .expect("render expected HTML")
}

#[test]
fn ui_worker_js_input_value_updates_pixels() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #x {
        position: absolute;
        left: 0;
        top: 0;
        width: 220px;
        height: 40px;
        font-size: 32px;
      }
    </style>
  </head>
  <body>
    <input id="x" value="a">
    <script>
      // Ensure the update happens on a later task turn so the first rendered frame shows "a".
      setTimeout(() => x.value = "b", 0);
    </script>
  </body>
</html>"#,
  );

  let viewport_css = (240, 60);
  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-js-input-value",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, viewport_css, 1.0))
    .expect("viewport");

  let _frame_before = next_frame(&ui_rx, tab_id);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");
  let frame_after = next_frame(&ui_rx, tab_id);

  let expected = render_expected(
    viewport_css,
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #x {
        position: absolute;
        left: 0;
        top: 0;
        width: 220px;
        height: 40px;
        font-size: 32px;
      }
    </style>
  </head>
  <body>
    <input id="x" value="b">
  </body>
</html>"#,
  );

  assert_eq!(
    frame_after.pixmap.data(),
    expected.data(),
    "expected JS `input.value` update to be reflected in UI worker pixels"
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_js_checkbox_checked_updates_pixels() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #c {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
      }
    </style>
  </head>
  <body>
    <input id="c" type="checkbox">
    <script>
      setTimeout(() => c.checked = true, 0);
    </script>
  </body>
</html>"#,
  );

  let viewport_css = (64, 64);
  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-js-checkbox-checked",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, viewport_css, 1.0))
    .expect("viewport");

  let _frame_before = next_frame(&ui_rx, tab_id);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");
  let frame_after = next_frame(&ui_rx, tab_id);

  let expected = render_expected(
    viewport_css,
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #c {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
      }
    </style>
  </head>
  <body>
    <input id="c" type="checkbox" checked>
  </body>
</html>"#,
  );

  assert_eq!(
    frame_after.pixmap.data(),
    expected.data(),
    "expected JS `checkbox.checked` update to be reflected in UI worker pixels"
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_js_textarea_value_updates_pixels() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #t {
        position: absolute;
        left: 0;
        top: 0;
        width: 240px;
        height: 80px;
        font-size: 24px;
      }
    </style>
  </head>
  <body>
    <textarea id="t"></textarea>
    <script>
      setTimeout(() => t.value = "hello", 0);
    </script>
  </body>
</html>"#,
  );

  let viewport_css = (260, 100);
  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-js-textarea-value",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, viewport_css, 1.0))
    .expect("viewport");

  let _frame_before = next_frame(&ui_rx, tab_id);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");
  let frame_after = next_frame(&ui_rx, tab_id);

  let expected = render_expected(
    viewport_css,
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #t {
        position: absolute;
        left: 0;
        top: 0;
        width: 240px;
        height: 80px;
        font-size: 24px;
      }
    </style>
  </head>
  <body>
    <textarea id="t" data-fastr-value="hello"></textarea>
  </body>
</html>"#,
  );

  assert_eq!(
    frame_after.pixmap.data(),
    expected.data(),
    "expected JS `textarea.value` update to be reflected in UI worker pixels"
  );

  drop(ui_tx);
  join.join().expect("worker join");
}
