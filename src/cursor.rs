use crate::style::types::CursorKeyword;

/// High-level pointer cursor semantics.
///
/// This intentionally mirrors a small subset of common browser cursor types so embedders (desktop UI,
/// IPC protocols, etc.) can map them to platform cursor icons without needing to understand the full
/// CSS cursor keyword space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorKind {
  Default,
  /// Hide the OS cursor (CSS `cursor: none`).
  Hidden,
  Pointer,
  Text,
  Crosshair,
  NotAllowed,
  Grab,
  Grabbing,
  Help,
  Wait,
  Progress,
  Move,
  Copy,
  ZoomIn,
  ZoomOut,
  EwResize,
  NsResize,
}

impl Default for CursorKind {
  fn default() -> Self {
    CursorKind::Default
  }
}

impl CursorKind {
  /// Map a CSS [`CursorKeyword`] to a high-level [`CursorKind`].
  ///
  /// Returns `None` for [`CursorKeyword::Auto`], which indicates the UA should choose a cursor based
  /// on the hovered element's semantics (e.g. links → pointer, selectable text → text caret).
  pub fn from_css_cursor_keyword(keyword: CursorKeyword) -> Option<Self> {
    match keyword {
      CursorKeyword::Auto => None,
      CursorKeyword::Default => Some(CursorKind::Default),
      CursorKeyword::None => Some(CursorKind::Hidden),
      CursorKeyword::Help => Some(CursorKind::Help),
      CursorKeyword::Pointer => Some(CursorKind::Pointer),
      CursorKeyword::Text | CursorKeyword::VerticalText => Some(CursorKind::Text),
      CursorKeyword::Crosshair => Some(CursorKind::Crosshair),
      CursorKeyword::NotAllowed | CursorKeyword::NoDrop => Some(CursorKind::NotAllowed),
      CursorKeyword::Grab => Some(CursorKind::Grab),
      CursorKeyword::Grabbing => Some(CursorKind::Grabbing),
      CursorKeyword::Wait => Some(CursorKind::Wait),
      CursorKeyword::Progress => Some(CursorKind::Progress),
      CursorKeyword::Move | CursorKeyword::AllScroll => Some(CursorKind::Move),
      CursorKeyword::Copy | CursorKeyword::Alias => Some(CursorKind::Copy),
      CursorKeyword::ZoomIn => Some(CursorKind::ZoomIn),
      CursorKeyword::ZoomOut => Some(CursorKind::ZoomOut),
      CursorKeyword::NResize | CursorKeyword::SResize => Some(CursorKind::NsResize),
      CursorKeyword::EResize | CursorKeyword::WResize => Some(CursorKind::EwResize),
      CursorKeyword::EwResize | CursorKeyword::ColResize => Some(CursorKind::EwResize),
      CursorKeyword::NsResize | CursorKeyword::RowResize => Some(CursorKind::NsResize),
      // Degrade gracefully for cursor keywords that do not have a dedicated `CursorKind` variant.
      _ => Some(CursorKind::Default),
    }
  }
}
