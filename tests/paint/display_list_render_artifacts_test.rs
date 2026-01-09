use fastrender::paint::display_list::{BlendMode, DisplayItem};
use fastrender::text::font_db::FontConfig;
use fastrender::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions,
};

#[test]
fn render_artifacts_display_list_uses_stacking_context_builder() {
  // `RenderArtifacts.display_list` is intended to reflect the display list fed into the paint
  // backend. That requires a stacking-context-aware build so compositing primitives like
  // `mix-blend-mode` are represented as `PushStackingContext` items.
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      .backdrop { width: 32px; height: 32px; background: rgb(255 0 0); }
      .blend { width: 32px; height: 32px; background: rgb(0 0 255); mix-blend-mode: color-burn; }
    </style>
    <div class="backdrop"><div class="blend"></div></div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });

  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");

  let list = artifacts.display_list.take().expect("display list artifact");
  assert!(
    list.items().iter().any(|item| matches!(
      item,
      DisplayItem::PushStackingContext(sc) if sc.mix_blend_mode == BlendMode::ColorBurn
    )),
    "expected display list artifact to contain a PushStackingContext for `mix-blend-mode: color-burn`"
  );
}

