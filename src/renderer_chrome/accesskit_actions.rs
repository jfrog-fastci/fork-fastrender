use crate::api::BrowserTab;
use crate::dom2::NodeId;
use crate::error::Result;
use crate::geometry::{Point, Rect};
use crate::ui::PointerModifiers;

/// Route an AccessKit action request to a DOM event (renderer-chrome).
///
/// Returns `Ok(Some(default_allowed))` when the action was handled, where `default_allowed` is
/// `true` when the dispatched DOM event's default was **not** prevented.
///
/// Returns `Ok(None)` when the action is not handled by this router.
pub fn route_accesskit_action_to_dom(
  tab: &mut BrowserTab,
  action: accesskit::Action,
  target_dom: NodeId,
  bounds_css: Option<Rect>,
  modifiers: PointerModifiers,
) -> Result<Option<bool>> {
  match action {
    // Assistive technologies can request a context menu without pointer input. Surface that as a
    // trusted DOM `contextmenu` event so chrome JS can react exactly like a right-click.
    accesskit::Action::ShowContextMenu => {
      // Prefer the center of the accessible bounds; this mirrors how a mouse-driven context menu
      // targets the element under the pointer. When bounds are unavailable, fall back to the
      // origin (0,0) so scripts still see a consistent MouseEvent shape.
      let pos = bounds_css.map(Rect::center).unwrap_or(Point::ZERO);
      let allowed =
        tab.dispatch_contextmenu_event_with_pointer(target_dom, (pos.x, pos.y), modifiers)?;
      Ok(Some(allowed))
    }
    _ => Ok(None),
  }
}
