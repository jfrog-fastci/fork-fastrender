use fastrender::chrome_frame::{ChromeFrameDocument, ChromeFrameEvent};
use fastrender::text::font_db::FontConfig;
use fastrender::{FastRender, LayoutParallelism, PaintParallelism, RenderOptions};

#[test]
fn chrome_frame_document_ime_preedit_commit_cancel_roundtrip() {
  let renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("deterministic renderer");

  let options = RenderOptions::new()
    .with_viewport(240, 64)
    .with_device_pixel_ratio(1.0)
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());

  let mut doc =
    ChromeFrameDocument::new_with_renderer_and_options(renderer, options).expect("chrome doc");

  let _ = doc.render_frame().expect("initial frame");

  // Focus the input, then render so subsequent repaint checks isolate IME changes.
  let focus_events = doc.focus_address_bar();
  assert!(
    focus_events
      .iter()
      .any(|e| matches!(e, ChromeFrameEvent::AddressBarFocusChanged(true))),
    "expected focus event when focusing address bar, got {focus_events:?}"
  );
  let focused_pixmap = doc
    .render_if_needed()
    .expect("render after focus")
    .expect("expected focus to trigger repaint");
  let focused_bytes = focused_pixmap.data().to_vec();

  // Preedit should repaint and should not mutate the DOM value.
  assert!(doc.ime_preedit("あ", Some((0, 1))));
  assert_eq!(doc.address_bar_value(), "");

  let preedit_pixmap = doc
    .render_if_needed()
    .expect("render after preedit")
    .expect("expected preedit to trigger repaint");
  assert_ne!(
    focused_bytes,
    preedit_pixmap.data(),
    "expected IME preedit to affect rendered pixels"
  );

  // Cancel clears preedit without touching the DOM value.
  assert!(doc.ime_cancel());
  assert!(doc.interaction_state().ime_preedit.is_none());
  assert_eq!(doc.address_bar_value(), "");

  let _ = doc
    .render_if_needed()
    .expect("render after cancel")
    .expect("expected cancel to trigger repaint");

  // Preedit again, then commit: DOM value should be updated.
  assert!(doc.ime_preedit("あ", Some((0, 1))));
  let _ = doc
    .render_if_needed()
    .expect("render after preedit 2")
    .expect("expected preedit to trigger repaint");

  let commit_events = doc.ime_commit("あ");
  assert!(doc.interaction_state().ime_preedit.is_none());
  assert!(
    commit_events.iter().any(|event| matches!(
      event,
      ChromeFrameEvent::AddressBarTextChanged(text) if text == "あ"
    )),
    "expected commit to emit AddressBarTextChanged(\"あ\"), got {commit_events:?}"
  );
  assert_eq!(doc.address_bar_value(), "あ");
}
