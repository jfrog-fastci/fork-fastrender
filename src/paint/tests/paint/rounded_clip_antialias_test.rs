use crate::geometry::Rect;
use crate::paint::canvas::Canvas;
use crate::paint::display_list::BorderRadii;
use crate::Rgba;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn rounded_clip_is_antialiased() {
  let mut canvas = Canvas::new(12, 12, Rgba::WHITE).unwrap();
  canvas
    .set_clip_with_radii(
      Rect::from_xywh(2.0, 2.0, 8.0, 8.0),
      Some(BorderRadii::uniform(4.0)),
    )
    .unwrap();

  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 12.0, 12.0), Rgba::BLACK);
  let pixmap = canvas.into_pixmap();

  assert_eq!(pixel(&pixmap, 6, 6), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 2, 2), (255, 255, 255, 255));

  let edge = pixel(&pixmap, 2, 3);
  assert!(
    edge.0 > 0 && edge.0 < 255,
    "expected anti-aliased edge pixel, got {edge:?}"
  );
  assert_eq!(edge.0, edge.1);
  assert_eq!(edge.1, edge.2);
  assert_eq!(edge.3, 255);
}
