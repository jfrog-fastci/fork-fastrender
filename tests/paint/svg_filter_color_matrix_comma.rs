use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use fastrender::Rect;
use tiny_skia::{ColorU8, Pixmap};

#[test]
fn fe_color_matrix_matrix_parses_comma_separated_values() {
  let cache = ImageCache::new();
  let svg = concat!(
    "<svg xmlns='http://www.w3.org/2000/svg'>",
    "<filter id='f' filterUnits='userSpaceOnUse' primitiveUnits='userSpaceOnUse' ",
    "x='0' y='0' width='1' height='1' color-interpolation-filters='sRGB'>",
    // Swap R and B channels.
    "<feColorMatrix type='matrix' values='0,0,1,0,0, 0,1,0,0,0, 1,0,0,0,0, 0,0,0,1,0'/>",
    "</filter>",
    "</svg>"
  );
  let filter = parse_svg_filter_from_svg_document(svg, Some("f"), &cache).expect("parsed filter");

  let mut pixmap = Pixmap::new(1, 1).expect("pixmap");
  pixmap.pixels_mut()[0] = ColorU8::from_rgba(10, 20, 30, 255).premultiply();
  let before = pixmap.pixels()[0];

  let bbox = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
  apply_svg_filter(filter.as_ref(), &mut pixmap, 1.0, bbox).unwrap();

  let after = pixmap.pixels()[0];
  assert_ne!(
    (after.red(), after.green(), after.blue(), after.alpha()),
    (before.red(), before.green(), before.blue(), before.alpha()),
    "expected comma-separated matrix values to affect output"
  );
  assert_eq!(
    (after.red(), after.green(), after.blue(), after.alpha()),
    (30, 20, 10, 255)
  );
}
