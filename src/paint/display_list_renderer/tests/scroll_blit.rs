use crate::paint::display_list::{DisplayItem, DisplayList, FillRectItem};
use crate::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use crate::text::font_loader::FontContext;
use crate::{Point, Rect, Rgba};

const VIEWPORT_W: u32 = 64;
const VIEWPORT_H: u32 = 64;
const WORLD_W: u32 = 80;
const WORLD_H: u32 = 80;
const DAMAGE_TILE_SIZE: u32 = 8;

fn channel(v: u32) -> u8 {
  ((v % 255) + 1) as u8
}

fn grid_color(x: u32, y: u32) -> Rgba {
  // Encode each axis into a separate channel to ensure axis mixups show up, while avoiding black
  // so missing repaints don't blend in with the background.
  let r = channel(x.wrapping_mul(37).wrapping_add(11));
  let g = channel(y.wrapping_mul(53).wrapping_add(29));
  let b = channel((x ^ y).wrapping_mul(11).wrapping_add(7));
  Rgba::rgb(r, g, b)
}

fn build_scrolled_grid(scroll: Point) -> DisplayList {
  let mut items = Vec::with_capacity((WORLD_W * WORLD_H) as usize);
  for y in 0..WORLD_H {
    for x in 0..WORLD_W {
      items.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(x as f32 - scroll.x, y as f32 - scroll.y, 1.0, 1.0),
        color: grid_color(x, y),
      }));
    }
  }
  DisplayList::from_items(items)
}

fn render_full(list: &DisplayList) -> tiny_skia::Pixmap {
  let mut renderer = DisplayListRenderer::new(VIEWPORT_W, VIEWPORT_H, Rgba::BLACK, FontContext::new())
    .expect("renderer");
  renderer.paint_parallelism = PaintParallelism::disabled();
  renderer
    .render_with_report(list)
    .expect("full render")
    .pixmap
}

fn assert_scroll_blit_matches_full(scroll_a: Point, scroll_b: Point) {
  let list_a = build_scrolled_grid(scroll_a);
  let list_b = build_scrolled_grid(scroll_b);

  let pixmap_a = render_full(&list_a);

  let delta = Point::new(scroll_b.x - scroll_a.x, scroll_b.y - scroll_a.y);
  let report = {
    let mut renderer = DisplayListRenderer::new_from_existing_pixmap(
      pixmap_a,
      Rgba::BLACK,
      FontContext::new(),
    )
    .expect("renderer");
    renderer.paint_parallelism = PaintParallelism::disabled();
    renderer.paint_parallelism.tile_size = DAMAGE_TILE_SIZE;
    renderer
      .render_scroll_blit_with_report(&list_b, delta)
      .expect("scroll blit render")
  };

  assert!(
    report.scroll_blit_used,
    "expected scroll blit path for scroll_a={:?}, scroll_b={:?} (fallback={:?})",
    scroll_a,
    scroll_b,
    report.fallback_reason
  );
  assert!(
    report.partial_repaint_used,
    "expected stripe repaint path for scroll_a={:?}, scroll_b={:?} (fallback={:?})",
    scroll_a,
    scroll_b,
    report.fallback_reason
  );

  let pixmap_b = render_full(&list_b);
  assert_eq!(
    report.pixmap.data(),
    pixmap_b.data(),
    "optimized scroll blit output did not match full repaint for scroll_a={:?}, scroll_b={:?}",
    scroll_a,
    scroll_b
  );
}

#[test]
fn scroll_blit_horizontal_positive_delta_matches_full() {
  assert_scroll_blit_matches_full(Point::new(0.0, 0.0), Point::new(9.0, 0.0));
}

#[test]
fn scroll_blit_vertical_positive_delta_matches_full() {
  assert_scroll_blit_matches_full(Point::new(0.0, 0.0), Point::new(0.0, 9.0));
}

#[test]
fn scroll_blit_diagonal_positive_delta_matches_full() {
  assert_scroll_blit_matches_full(Point::new(0.0, 0.0), Point::new(9.0, 9.0));
}

#[test]
fn scroll_blit_horizontal_negative_delta_matches_full() {
  assert_scroll_blit_matches_full(Point::new(9.0, 0.0), Point::new(0.0, 0.0));
}

#[test]
fn scroll_blit_vertical_negative_delta_matches_full() {
  assert_scroll_blit_matches_full(Point::new(0.0, 9.0), Point::new(0.0, 0.0));
}

#[test]
fn scroll_blit_diagonal_negative_delta_matches_full() {
  assert_scroll_blit_matches_full(Point::new(9.0, 9.0), Point::new(0.0, 0.0));
}

