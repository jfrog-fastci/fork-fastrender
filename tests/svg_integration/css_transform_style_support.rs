use resvg::usvg;
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};

#[test]
fn resvg_ignores_css_transform_translate_percent() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="30" height="10" viewBox="0 0 30 10" shape-rendering="crispEdges">
      <g style="transform-box: fill-box; transform: translateX(100%);">
        <rect width="10" height="10" fill="rgb(255,0,0)" />
      </g>
    </svg>
  "#;

  let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).expect("parse svg");
  let mut pixmap = Pixmap::new(30, 10).expect("pixmap");
  resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());

  let red = PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("color");
  let pixels = pixmap.pixels_mut();
  let at = |x: u32, y: u32| pixels[(y * 30 + x) as usize];

  assert_eq!(
    at(5, 5),
    red,
    "resvg/usvg currently ignores CSS `transform` in the style attribute (percent case)"
  );
  assert_eq!(
    at(15, 5),
    PremultipliedColorU8::TRANSPARENT,
    "rect should remain at the origin when CSS `transform` is ignored"
  );
}
