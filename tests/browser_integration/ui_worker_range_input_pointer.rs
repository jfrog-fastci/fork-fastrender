#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RenderedFrame, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn extract_frame(messages: Vec<WorkerToUi>) -> Option<RenderedFrame> {
  messages.into_iter().rev().find_map(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  })
}

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  support::rgba_at(&frame.pixmap, x_px, y_px)
}

#[test]
fn range_input_pointer_drag_updates_value_and_clamps() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (320, 140);
  let url = "https://example.com/index.html";

  // Place the slider at a known position/width so pointer x coordinates map cleanly to values.
  // A sibling probe element changes color based on the input's value attribute so we can assert
  // both DOM mutation and repaint.
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }

          #r {
            position: absolute;
            left: 50px;
            top: 30px;
            width: 200px;
            height: 30px;
            box-sizing: border-box;
            border: 0;
            padding: 0;
            margin: 0;
          }

          #probe {
            position: absolute;
            left: 0;
            top: 80px;
            width: 40px;
            height: 40px;
            background: rgb(255, 0, 0); /* value=0 */
          }

          input[value="50"] ~ #probe { background: rgb(0, 255, 0); }
          input[value="100"] ~ #probe { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <input id="r" type="range" min="0" max="100" step="1" value="0">
        <div id="probe"></div>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;

  // Initial paint.
  let frame0 =
    extract_frame(controller.handle_message(UiToWorker::RequestRepaint {
      tab_id,
      reason: RepaintReason::Explicit,
    })?)
    .expect("expected initial FrameReady");
  assert_eq!(rgba_at_css(&frame0, 10, 90), [255, 0, 0, 255]);

  // -----------------------------------------------------------------------------
  // Drag from start to ~50% width: expect value=50.
  // Slider: left=50, width=200 => 50% at x=150.
  // -----------------------------------------------------------------------------
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (50.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let frame50 = extract_frame(controller.handle_message(UiToWorker::PointerMove {
    tab_id,
    pos_css: (150.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after dragging range to 50%");
  assert_eq!(rgba_at_css(&frame50, 10, 90), [0, 255, 0, 255]);

  let frame50_up = extract_frame(controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (150.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after PointerUp");
  assert_eq!(rgba_at_css(&frame50_up, 10, 90), [0, 255, 0, 255]);

  // -----------------------------------------------------------------------------
  // Drag beyond the right edge: expect clamping to max=100.
  // Slider right edge is x=250; drag to x=290 (still inside viewport).
  // -----------------------------------------------------------------------------
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (150.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let frame_max = extract_frame(controller.handle_message(UiToWorker::PointerMove {
    tab_id,
    pos_css: (290.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after dragging range past end");
  assert_eq!(rgba_at_css(&frame_max, 10, 90), [0, 0, 255, 255]);

  let frame_max_up = extract_frame(controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (290.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after PointerUp at max");
  assert_eq!(rgba_at_css(&frame_max_up, 10, 90), [0, 0, 255, 255]);

  // -----------------------------------------------------------------------------
  // Drag beyond the left edge: expect clamping to min=0.
  // Slider left edge is x=50; drag to x=10 (still inside viewport).
  // -----------------------------------------------------------------------------
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (250.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let frame_min = extract_frame(controller.handle_message(UiToWorker::PointerMove {
    tab_id,
    pos_css: (10.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after dragging range past start");
  assert_eq!(rgba_at_css(&frame_min, 10, 90), [255, 0, 0, 255]);

  let frame_min_up = extract_frame(controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 45.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after PointerUp at min");
  assert_eq!(rgba_at_css(&frame_min_up, 10, 90), [255, 0, 0, 255]);

  Ok(())
}
