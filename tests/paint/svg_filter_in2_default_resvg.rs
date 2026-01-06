use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};

fn render_resvg(svg: &str, width: u32, height: u32) -> Pixmap {
  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;
  let tree = resvg::usvg::Tree::from_str(svg, &options).expect("parse svg");

  let mut pixmap = Pixmap::new(width, height).expect("pixmap");

  let size = tree.size();
  let scale_x = width as f32 / size.width() as f32;
  let scale_y = height as f32 / size.height() as f32;
  let transform = Transform::from_scale(scale_x, scale_y);

  resvg::render(&tree, transform, &mut pixmap.as_mut());
  pixmap
}

fn rect_source_pixmap(width: u32, height: u32) -> Pixmap {
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  let blue = PremultipliedColorU8::from_rgba(0, 0, 255, 255).expect("blue");
  let stride = width as usize;
  for y in 1..height.saturating_sub(1) {
    for x in 1..width.saturating_sub(1) {
      pixmap.pixels_mut()[y as usize * stride + x as usize] = blue;
    }
  }
  pixmap
}

fn row_source_pixmap(colors: &[[u8; 3]]) -> Pixmap {
  let mut pixmap = Pixmap::new(colors.len() as u32, 1).expect("pixmap");
  for (idx, [r, g, b]) in colors.iter().copied().enumerate() {
    pixmap.pixels_mut()[idx] = PremultipliedColorU8::from_rgba(r, g, b, 255).expect("color");
  }
  pixmap
}

fn assert_pixmaps_close(actual: &Pixmap, expected: &Pixmap, tolerance: u8) {
  assert_eq!(
    (actual.width(), actual.height()),
    (expected.width(), expected.height()),
    "pixmap dimensions differ",
  );

  let mut worst = None::<(u32, u32, usize, u8, u8)>;
  let mut max_diff = 0u8;
  for y in 0..expected.height() {
    for x in 0..expected.width() {
      let idx = ((y * expected.width() + x) * 4) as usize;
      for c in 0..4 {
        let a = actual.data()[idx + c];
        let b = expected.data()[idx + c];
        let diff = a.abs_diff(b);
        if diff > max_diff {
          max_diff = diff;
          worst = Some((x, y, c, a, b));
        }
      }
    }
  }

  assert!(
    max_diff <= tolerance,
    "pixmaps differ (max diff {max_diff} > {tolerance}); worst at {:?}",
    worst
  );
}

#[test]
fn missing_in2_defaults_match_resvg_for_fe_composite() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="4" viewBox="0 0 4 4">
      <defs>
        <filter id="f" filterUnits="userSpaceOnUse" x="0" y="0" width="4" height="4"
                color-interpolation-filters="sRGB">
          <feFlood flood-color="rgb(255,0,0)" result="a"/>
          <feFlood flood-color="rgb(0,255,0)" flood-opacity="0" result="b"/>
          <feComposite in="a" operator="out"/>
        </filter>
      </defs>
      <rect x="1" y="1" width="2" height="2" fill="rgb(0,0,255)" filter="url(#f)"/>
    </svg>
  "#;

  let expected = render_resvg(svg, 4, 4);

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("parse filter");

  let mut actual = rect_source_pixmap(4, 4);
  let bbox = Rect::from_xywh(0.0, 0.0, 4.0, 4.0);
  apply_svg_filter(&filter, &mut actual, 1.0, bbox).expect("apply filter");

  assert_pixmaps_close(&actual, &expected, 0);
}

#[test]
fn missing_in2_defaults_match_resvg_for_fe_blend() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="4" viewBox="0 0 4 4">
      <defs>
        <filter id="f" filterUnits="userSpaceOnUse" x="0" y="0" width="4" height="4"
                color-interpolation-filters="sRGB">
          <feFlood flood-color="rgb(255,0,0)" result="a"/>
          <feFlood flood-color="rgb(0,255,0)" result="b"/>
          <feBlend in="a" mode="difference"/>
        </filter>
      </defs>
      <rect x="1" y="1" width="2" height="2" fill="rgb(0,0,255)" filter="url(#f)"/>
    </svg>
  "#;

  let expected = render_resvg(svg, 4, 4);

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("parse filter");

  let mut actual = rect_source_pixmap(4, 4);
  let bbox = Rect::from_xywh(0.0, 0.0, 4.0, 4.0);
  apply_svg_filter(&filter, &mut actual, 1.0, bbox).expect("apply filter");

  assert_pixmaps_close(&actual, &expected, 0);
}

#[test]
fn missing_in2_defaults_match_resvg_for_theverge_style_composite_chain() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="4" viewBox="0 0 4 4">
      <defs>
        <filter id="f" filterUnits="userSpaceOnUse" x="0" y="0" width="4" height="4"
                color-interpolation-filters="sRGB">
          <feGaussianBlur stdDeviation="1"/>
          <feColorMatrix values="1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 100 -1" result="s"/>
          <feFlood x="0" y="0" width="100%" height="100%"/>
          <feComposite operator="out" in="s"/>
          <feComposite in2="SourceGraphic"/>
        </filter>
      </defs>
      <rect x="1" y="1" width="2" height="2" fill="rgb(0,0,255)" filter="url(#f)"/>
    </svg>
  "#;

  let expected = render_resvg(svg, 4, 4);

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("parse filter");

  let mut actual = rect_source_pixmap(4, 4);
  let bbox = Rect::from_xywh(0.0, 0.0, 4.0, 4.0);
  apply_svg_filter(&filter, &mut actual, 1.0, bbox).expect("apply filter");

  assert_pixmaps_close(&actual, &expected, 0);
}

#[test]
fn missing_in2_defaults_match_resvg_for_fe_displacement_map() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="4" height="1" viewBox="0 0 4 1">
      <defs>
        <filter id="f" filterUnits="userSpaceOnUse" x="0" y="0" width="4" height="1"
                color-interpolation-filters="sRGB">
          <feFlood flood-color="rgb(255,0,0)"/>
          <feDisplacementMap in="SourceGraphic" scale="2" xChannelSelector="R" yChannelSelector="R"/>
        </filter>
      </defs>
      <g filter="url(#f)">
        <rect x="0" y="0" width="1" height="1" fill="rgb(255,0,0)"/>
        <rect x="1" y="0" width="1" height="1" fill="rgb(0,255,0)"/>
        <rect x="2" y="0" width="1" height="1" fill="rgb(0,0,255)"/>
        <rect x="3" y="0" width="1" height="1" fill="rgb(255,255,0)"/>
      </g>
    </svg>
  "#;

  let expected = render_resvg(svg, 4, 1);

  let filter =
    parse_svg_filter_from_svg_document(svg, Some("f"), &ImageCache::new()).expect("parse filter");

  let mut actual = row_source_pixmap(&[[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]]);
  let bbox = Rect::from_xywh(0.0, 0.0, 4.0, 1.0);
  apply_svg_filter(&filter, &mut actual, 1.0, bbox).expect("apply filter");

  assert_pixmaps_close(&actual, &expected, 0);
}
