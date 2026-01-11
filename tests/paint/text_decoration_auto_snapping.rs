use fastrender::paint::display_list::DisplayItem;
use fastrender::text::font_db::FontConfig;
use fastrender::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions,
};

#[test]
fn text_decoration_auto_uses_font_thickness_and_snaps_solid_underlines() {
  crate::rayon_test_util::init_rayon_for_tests(2);

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; font-family: sans-serif; font-size: 16px; }
          .sample {
            color: black;
            text-decoration: underline;
            text-decoration-color: rgb(0, 0, 255);
            text-decoration-skip-ink: none;
            line-height: 24px;
          }
          .auto { text-decoration-thickness: auto; }
          .from { text-decoration-thickness: from-font; }
        </style>
      </head>
      <body>
        <div class="sample auto">ABC</div>
        <div class="sample from">ABC</div>
      </body>
    </html>
  "#;

  let options = RenderOptions::new()
    .with_viewport(200, 120)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());

  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });

  let pixmap = renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render");
  let display_list = artifacts.display_list.take().expect("display list");

  let mut underline_thicknesses: Vec<(f32, f32)> = display_list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TextDecoration(decoration) => {
        let stroke = decoration
          .decorations
          .iter()
          .find_map(|paint| paint.underline.as_ref());
        stroke.map(|stroke| (decoration.bounds.y(), stroke.thickness))
      }
      _ => None,
    })
    .collect();

  underline_thicknesses.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
  assert!(
    underline_thicknesses.len() >= 2,
    "expected >=2 underline decorations, found {}",
    underline_thicknesses.len()
  );
  let auto_thickness = underline_thicknesses[0].1;
  let from_font_thickness = underline_thicknesses[1].1;
  assert!(
    (auto_thickness - from_font_thickness).abs() < 1e-3,
    "expected auto underline thickness ({auto_thickness}) to match from-font ({from_font_thickness})"
  );

  let mut blue_pixels = 0usize;
  let mut non_solid_blue_pixels = 0usize;
  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let p = pixmap.pixel(x, y).expect("pixel");
      let (r, g, b, a) = (p.red(), p.green(), p.blue(), p.alpha());
      if a == 0 {
        continue;
      }
      // Identify underline pixels by looking for strong blue dominance.
      if b > 200 && b > r.saturating_add(50) && b > g.saturating_add(50) {
        blue_pixels += 1;
        if r > 0 || g > 0 {
          non_solid_blue_pixels += 1;
        }
      }
    }
  }
  assert!(blue_pixels > 0, "expected some underline pixels to be painted");
  assert_eq!(
    non_solid_blue_pixels, 0,
    "expected solid underline pixels to be pure blue (no subpixel AA), found {non_solid_blue_pixels}/{blue_pixels}"
  );
}

