use fastrender::{
  BrowserDocumentDom2, LayoutParallelism, PaintParallelism, PreparedPaintOptions, RenderOptions,
  Result,
};

#[test]
fn scroll_blit_disabled_when_animation_time_changes() -> Result<()> {
  // This mirrors the stale-pixels failure mode for scroll-blit:
  //
  // 1) Render a frame at animation_time = 0ms.
  // 2) Advance animation_time and trigger a small scroll delta.
  // 3) Paint from cache (eligible for scroll-blit in future).
  // 4) Assert the scroll repaint matches a fresh full repaint at the new animation time.
  //
  // Without an animation-time gate, a scroll-blit implementation that only repaints exposed stripes
  // would reuse stale pixels for the old animation timestamp.
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      #anim {
        width: 64px;
        height: 128px;
        background: rgb(255, 0, 0);
        animation: fade 1000ms linear forwards;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
      .tail { height: 512px; }
    </style>
    <div id="anim"></div>
    <div class="tail"></div>
  "#;

  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  let mut doc = BrowserDocumentDom2::from_html(html, options)?;

  doc.set_animation_time_ms(0.0);
  let _ = doc.render_frame()?;

  doc.set_animation_time_ms(500.0);
  doc.set_scroll(0.0, 5.0);
  let scrolled = doc.paint_from_cache_frame_with_deadline(None)?;

  let prepared = doc.prepared().expect("prepared document");
  let expected = prepared.paint_with_options_frame(PreparedPaintOptions {
    scroll: Some(scrolled.scroll_state.clone()),
    viewport: None,
    background: None,
    animation_time: Some(500.0),
    ..PreparedPaintOptions::default()
  })?;

  assert_eq!(
    scrolled.pixmap.data(),
    expected.pixmap.data(),
    "scroll repaint did not match a full repaint; scroll-blit may have reused stale pixels due to an animation time change"
  );

  Ok(())
}

