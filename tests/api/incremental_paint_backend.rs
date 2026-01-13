use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{
  BrowserDocument2, IncrementalPaintDisabledReason, LayoutParallelism, PaintParallelism,
  RenderOptions, Result,
};
use std::collections::HashMap;

#[test]
fn render_frame_into_disables_incremental_paint_when_backend_is_legacy() -> Result<()> {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_runtime_toggles(toggles);

  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      #box { width: 32px; height: 32px; background: rgb(255, 0, 0); }
    </style>
    <div id="box"></div>
  "#;

  // Baseline: full repaint.
  let mut baseline = BrowserDocument2::from_html(html, options.clone())?;
  let expected = baseline.render_frame()?;

  // Incremental entrypoint: into a reusable buffer.
  let mut doc = BrowserDocument2::from_html(html, options)?;
  let mut output: Option<fastrender::Pixmap> = None;
  let _ = doc.render_frame_into(&mut output)?;
  let actual = output.as_ref().expect("render_frame_into output");

  assert_eq!(actual.data(), expected.data());

  let report = doc
    .last_incremental_paint_report()
    .expect("expected an incremental paint report after render_frame_into");
  assert!(!report.incremental_used);
  assert_eq!(
    report.disabled_reason,
    Some(IncrementalPaintDisabledReason::PaintBackendLegacy)
  );
  assert_eq!(report.disabled_reason_message(), Some("paint backend is legacy"));

  Ok(())
}

