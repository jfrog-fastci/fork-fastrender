use crate::style::types::CursorKeyword;
use crate::ui::messages::CursorKind;

use super::hit_test::{HitTestKind, HitTestResult};

/// Determine the desired UI cursor kind for a hit-tested target.
///
/// This is the single authoritative helper for mapping hit-test results to [`CursorKind`].
///
/// Resolution order:
/// 1. Respect the computed CSS `cursor` keyword when it is a concrete value (including `none`).
/// 2. For `cursor: auto`, apply browser-like heuristics (pointer for links, I-beam for selectable
///    text and text-like form controls).
pub fn cursor_kind_for_hit(hit: Option<&HitTestResult>) -> CursorKind {
  let Some(hit) = hit else {
    return CursorKind::Default;
  };

  // A concrete `cursor` value (including ones that degrade to `CursorKind::Default`) must suppress
  // `cursor: auto` heuristics, so we treat any `Some(...)` return as authoritative.
  if let Some(kind) = CursorKind::from_css_cursor_keyword(hit.css_cursor) {
    return kind;
  }

  // `cursor: auto` fallback semantics.
  if matches!(hit.kind, HitTestKind::Link) {
    return CursorKind::Pointer;
  }
  if matches!(hit.kind, HitTestKind::FormControl) {
    return hit.form_control_cursor;
  }
  if hit.is_selectable_text {
    return CursorKind::Text;
  }

  CursorKind::Default
}

#[cfg(test)]
mod tests {
  use super::*;

  fn hit(kind: HitTestKind) -> HitTestResult {
    HitTestResult {
      box_id: 1,
      css_cursor: CursorKeyword::Auto,
      is_selectable_text: false,
      dom_element_id: None,
      is_editable_text_drop_target_candidate: false,
      form_control_cursor: CursorKind::Default,
      styled_node_id: 1,
      dom_node_id: 1,
      kind,
      href: None,
    }
  }

  #[test]
  fn cursor_kind_for_hit_none_is_default() {
    assert_eq!(cursor_kind_for_hit(None), CursorKind::Default);
  }

  #[test]
  fn cursor_kind_for_hit_link_auto_is_pointer() {
    let hit = hit(HitTestKind::Link);
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Pointer);
  }

  #[test]
  fn cursor_kind_for_hit_link_respects_concrete_css_cursor() {
    let mut hit = hit(HitTestKind::Link);
    hit.css_cursor = CursorKeyword::Text;
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Text);
  }

  #[test]
  fn cursor_kind_for_hit_cursor_none_hides_cursor() {
    let mut hit = hit(HitTestKind::Other);
    hit.css_cursor = CursorKeyword::None;
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Hidden);
  }

  #[test]
  fn cursor_kind_for_hit_selectable_text_auto_is_text() {
    let mut hit = hit(HitTestKind::Other);
    hit.is_selectable_text = true;
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Text);
  }

  #[test]
  fn cursor_kind_for_hit_text_control_auto_is_text() {
    let mut hit = hit(HitTestKind::FormControl);
    hit.form_control_cursor = CursorKind::Text;
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Text);
  }

  #[test]
  fn cursor_kind_for_hit_unknown_concrete_cursor_suppresses_auto_fallback() {
    // Even though this is a link, a concrete `cursor` value (e.g. `wait`) should override link
    // heuristics.
    let mut hit = hit(HitTestKind::Link);
    hit.css_cursor = CursorKeyword::Wait;
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Default);
  }
}
