use crate::geometry::{Point, Rect};

/// Returns whether a pointer press should clear page (rendered content) focus.
///
/// The windowed browser UI treats the rendered page as a focusable widget so that keyboard input
/// can be routed to the page worker (scrolling, text entry, shortcuts). When the user clicks in the
/// browser chrome (address bar, toolbar, tabs, etc), we want to immediately stop forwarding keyboard
/// events to the page *even before* the next egui frame runs. Otherwise the first typed character
/// after a click can be misrouted to the page when the click and keypress arrive in a single winit
/// event batch.
pub fn should_clear_page_focus_on_pointer_press(
  page_rect_points: Option<Rect>,
  pos_points: Point,
) -> bool {
  let Some(rect) = page_rect_points else {
    return false;
  };
  !rect.contains_point(pos_points)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn click_outside_page_rect_clears_focus() {
    let rect = Rect::from_points(Point::new(0.0, 0.0), Point::new(100.0, 100.0));
    assert!(should_clear_page_focus_on_pointer_press(
      Some(rect),
      Point::new(150.0, 10.0)
    ));
  }

  #[test]
  fn click_inside_page_rect_keeps_focus() {
    let rect = Rect::from_points(Point::new(0.0, 0.0), Point::new(100.0, 100.0));
    assert!(!should_clear_page_focus_on_pointer_press(
      Some(rect),
      Point::new(50.0, 50.0)
    ));
  }

  #[test]
  fn unknown_page_rect_does_not_clear_focus() {
    assert!(!should_clear_page_focus_on_pointer_press(
      None,
      Point::new(0.0, 0.0)
    ));
  }
}
