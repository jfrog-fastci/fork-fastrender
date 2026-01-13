use super::support;
use fastrender::interaction::KeyAction;
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
fn range_input_keyboard_arrow_right_home_end_step_and_repaints() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (200, 140);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #lbl { position: absolute; left: 0; top: 0; width: 200px; height: 30px; background: rgb(240, 240, 240); }
          #r { position: absolute; left: 0; top: 30px; width: 200px; height: 30px; }
          #box { position: absolute; left: 0; top: 80px; width: 40px; height: 40px; background: rgb(255, 0, 0); }
          input[value="1"] ~ #box { background: rgb(0, 255, 0); }
          input[value="2"] ~ #box { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <label id="lbl" for="r">focus</label>
        <input id="r" type="range" min="0" max="2" step="1" value="0">
        <div id="box"></div>
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

  // Focus the range input by clicking its label (avoids changing its value by pointer position).
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let frame_after_focus = extract_frame(controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after focusing the label");
  assert_eq!(rgba_at_css(&frame_after_focus, 10, 90), [255, 0, 0, 255]);

  // ArrowRight increases by one step.
  let frame_after_right = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::ArrowRight,
  })?)
  .expect("expected FrameReady after ArrowRight");
  assert_eq!(rgba_at_css(&frame_after_right, 10, 90), [0, 255, 0, 255]);

  // End jumps to max.
  let frame_after_end = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::End,
  })?)
  .expect("expected FrameReady after End");
  assert_eq!(rgba_at_css(&frame_after_end, 10, 90), [0, 0, 255, 255]);

  // Home jumps to min.
  let frame_after_home = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Home,
  })?)
  .expect("expected FrameReady after Home");
  assert_eq!(rgba_at_css(&frame_after_home, 10, 90), [255, 0, 0, 255]);

  Ok(())
}

#[test]
fn range_input_keyboard_shift_variants_step_and_repaints() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (200, 140);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #lbl { position: absolute; left: 0; top: 0; width: 200px; height: 30px; background: rgb(240, 240, 240); }
          #r { position: absolute; left: 0; top: 30px; width: 200px; height: 30px; }
          #box { position: absolute; left: 0; top: 80px; width: 40px; height: 40px; background: rgb(255, 0, 0); }
          input[value="1"] ~ #box { background: rgb(0, 255, 0); }
          input[value="2"] ~ #box { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <label id="lbl" for="r">focus</label>
        <input id="r" type="range" min="0" max="2" step="1" value="0">
        <div id="box"></div>
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
  let frame0 = extract_frame(controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?)
  .expect("expected initial FrameReady");
  assert_eq!(rgba_at_css(&frame0, 10, 90), [255, 0, 0, 255]);

  // Focus the range input by clicking its label (avoids changing its value by pointer position).
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let frame_after_focus = extract_frame(controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?)
  .expect("expected FrameReady after focusing the label");
  assert_eq!(rgba_at_css(&frame_after_focus, 10, 90), [255, 0, 0, 255]);

  // Shift+ArrowUp should behave like ArrowUp for range inputs (step by one).
  let frame_after_up = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::ShiftArrowUp,
  })?)
  .expect("expected FrameReady after ShiftArrowUp");
  assert_eq!(rgba_at_css(&frame_after_up, 10, 90), [0, 255, 0, 255]);

  // Shift+ArrowDown should behave like ArrowDown (step back by one).
  let frame_after_down = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::ShiftArrowDown,
  })?)
  .expect("expected FrameReady after ShiftArrowDown");
  assert_eq!(rgba_at_css(&frame_after_down, 10, 90), [255, 0, 0, 255]);

  // Shift+ArrowRight should behave like ArrowRight for range inputs (step by one).
  let frame_after_right = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::ShiftArrowRight,
  })?)
  .expect("expected FrameReady after ShiftArrowRight");
  assert_eq!(rgba_at_css(&frame_after_right, 10, 90), [0, 255, 0, 255]);

  // Shift+End should behave like End (jump to max).
  let frame_after_end = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::ShiftEnd,
  })?)
  .expect("expected FrameReady after ShiftEnd");
  assert_eq!(rgba_at_css(&frame_after_end, 10, 90), [0, 0, 255, 255]);

  // Shift+Home should behave like Home (jump to min).
  let frame_after_home = extract_frame(controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::ShiftHome,
  })?)
  .expect("expected FrameReady after ShiftHome");
  assert_eq!(rgba_at_css(&frame_after_home, 10, 90), [255, 0, 0, 255]);

  Ok(())
}
