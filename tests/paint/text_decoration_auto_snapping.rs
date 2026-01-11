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

  // `TextDecoration` items can be split across runs; collapse items that share the same underline
  // position so the test logic can reason in terms of "one underline per line of text".
  let mut underlines: Vec<(f32, f32)> = Vec::new();
  for (y, thickness) in underline_thicknesses {
    match underlines.last() {
      Some((prev_y, _)) if (y - *prev_y).abs() < 1e-3 => {}
      _ => underlines.push((y, thickness)),
    }
  }
  assert!(
    underlines.len() >= 2,
    "expected >=2 underline decorations, found {}",
    underlines.len()
  );
  let auto_thickness = underlines[0].1;
  let from_font_thickness = underlines[1].1;
  assert!(
    (auto_thickness - from_font_thickness).abs() < 1e-3,
    "expected auto underline thickness ({auto_thickness}) to match from-font ({from_font_thickness})"
  );

  // Solid underlines are rendered as filled rectangles. When the canvas transform is translation-
  // only (typical text), we snap the rectangle to device pixels for crisp output. Importantly we
  // must not inflate a ~1.3px underline to 2 device pixels due to per-edge rounding; instead we
  // snap the origin and size independently.
  let expected_device_thicknesses: Vec<u32> = underlines
    .iter()
    .take(2)
    .map(|(_, thickness)| thickness.round().max(1.0) as u32)
    .collect();
  let expected_start_rows: Vec<u32> = underlines
    .iter()
    .take(2)
    .map(|(y, _)| y.round().max(0.0) as u32)
    .collect();

  let mut blue_pixels = 0usize;
  let mut non_solid_blue_pixels = 0usize;
  let mut blue_rows: Vec<u32> = Vec::new();
  for y in 0..pixmap.height() {
    let mut row_has_blue = false;
    for x in 0..pixmap.width() {
      let p = pixmap.pixel(x, y).expect("pixel");
      let (r, g, b, a) = (p.red(), p.green(), p.blue(), p.alpha());
      if a == 0 {
        continue;
      }
      // Identify underline pixels by looking for strong blue dominance.
      if b > 200 && b > r.saturating_add(50) && b > g.saturating_add(50) {
        blue_pixels += 1;
        row_has_blue = true;
        if r > 0 || g > 0 {
          non_solid_blue_pixels += 1;
        }
      }
    }
    if row_has_blue {
      blue_rows.push(y);
    }
  }
  assert!(blue_pixels > 0, "expected some underline pixels to be painted");
  assert_eq!(
    non_solid_blue_pixels, 0,
    "expected solid underline pixels to be pure blue (no subpixel AA), found {non_solid_blue_pixels}/{blue_pixels}"
  );

  // Group underline rows into clusters (one per underline).
  let mut clusters: Vec<(u32, u32)> = Vec::new();
  for y in blue_rows {
    match clusters.last_mut() {
      Some((_, end)) if y == *end + 1 => *end = y,
      _ => clusters.push((y, y)),
    }
  }

  for (idx, expected) in expected_device_thicknesses.iter().enumerate() {
    let expected_start = expected_start_rows[idx];
    let Some((start, end)) = clusters.iter().copied().find(|(start, _)| *start == expected_start)
    else {
      panic!(
        "expected underline band {idx} to start on row {expected_start}, but underline rows were {clusters:?}"
      );
    };
    let actual = end - start + 1;
    assert_eq!(
      actual, *expected,
      "expected underline band {idx} to have thickness {expected}px, got {actual}px (rows {start}..={end})"
    );
  }
}
