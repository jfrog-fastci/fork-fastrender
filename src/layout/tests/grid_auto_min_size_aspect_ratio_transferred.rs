use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::AspectRatio;
use fastrender::style::types::GridTrack;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

fn layout_grid_child_width_and_track_width(
  overflow_x: Overflow,
  max_width: Option<Length>,
) -> (f32, f32) {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(50.0));
  grid_style.grid_template_columns = vec![GridTrack::Fr(1.0)];
  grid_style.grid_template_rows = vec![GridTrack::Auto];

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.height = Some(Length::px(40.0));
  item_style.aspect_ratio = AspectRatio::Ratio(2.0);
  item_style.overflow_x = overflow_x;
  item_style.max_width = max_width;
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(50.0, 100.0))
    .expect("layout should succeed");

  let track_width = fragment
    .grid_tracks
    .as_ref()
    .and_then(|tracks| tracks.columns.first().copied())
    .map(|(start, end)| end - start)
    .unwrap_or(f32::NAN);

  let child = fragment.children.first().expect("child fragment");
  (child.bounds.width(), track_width)
}

#[test]
fn grid_auto_min_size_aspect_ratio_uses_transferred_size_suggestion() {
  let (width, track_width) = layout_grid_child_width_and_track_width(Overflow::Visible, None);
  assert!(
    width >= 79.0,
    "aspect-ratio grid item should overflow rather than shrink below transferred size suggestion; got width {width}"
  );
  assert!(
    track_width >= 79.0,
    "aspect-ratio grid item should force the 1fr track to at least the transferred size suggestion; got track width {track_width}"
  );
}

#[test]
fn grid_auto_min_size_aspect_ratio_transferred_suggestion_clamped_by_max_width() {
  let (width, track_width) =
    layout_grid_child_width_and_track_width(Overflow::Visible, Some(Length::px(60.0)));
  assert!(
    (width - 60.0).abs() < 0.5,
    "definite max-width should clamp the transferred size suggestion; expected ~60, got {width}"
  );
  assert!(
    (track_width - 60.0).abs() < 0.5,
    "definite max-width should clamp the transferred size suggestion for track sizing; expected ~60, got track width {track_width}"
  );
}

#[test]
fn grid_auto_min_size_overflow_hidden_disables_content_based_min_size() {
  let (width, track_width) = layout_grid_child_width_and_track_width(Overflow::Hidden, None);
  assert!(
    (track_width - 50.0).abs() < 0.5,
    "overflow: hidden should make the grid item a scroll container so auto min-size is 0; expected track ~50, got track width {track_width} (item width {width})"
  );
}
