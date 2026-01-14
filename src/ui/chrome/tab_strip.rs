use crate::ui::browser_app::{
  BrowserAppState, BrowserTabState, ChromeState, OpenTabContextMenuState, TabGroupColor,
  TabGroupId, TabGroupState, UiFocusToken,
};
use crate::ui::icons::paint_icon_in_rect;
use crate::ui::messages::TabId;
use crate::ui::motion::UiMotion;
use crate::ui::{icon_button_with_id, BrowserIcon};
use egui::{Align2, Color32, FontId, Pos2, Rect, Response, Sense, Stroke, Vec2};
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use super::FocusRingStyle;
use crate::ui::ChromeAction;

pub(super) fn tab_strip_tab_widget_id(tab_id: TabId) -> egui::Id {
  egui::Id::new(("tab_strip_tab", tab_id))
}

fn rect_from_points_tuple(r: (f32, f32, f32, f32)) -> Rect {
  Rect::from_min_max(Pos2::new(r.0, r.1), Pos2::new(r.2, r.3))
}

const TAB_STRIP_HEIGHT: f32 = 34.0;
const TAB_HEIGHT: f32 = 32.0;
const TAB_MIN_WIDTH: f32 = 140.0;
const TAB_MAX_WIDTH: f32 = 240.0;
const PINNED_TAB_WIDTH: f32 = 44.0;
const TAB_GAP: f32 = 6.0;
const TAB_PADDING_X: f32 = 10.0;
const GROUP_CHIP_MIN_WIDTH: f32 = 90.0;
const GROUP_CHIP_MAX_WIDTH: f32 = 180.0;
const GROUP_CHIP_PADDING_X: f32 = 10.0;
const GROUP_CHIP_ICON_SIZE: f32 = 10.0;
const GROUP_CHIP_ICON_GAP: f32 = 6.0;
const CONTROL_BUTTON_SIZE: f32 = 28.0;
const ICON_SIZE: f32 = 16.0;
const ICON_GAP: f32 = 8.0;
const CLOSE_BUTTON_SIZE: f32 = 28.0;
const ACTIVE_UNDERLINE_HEIGHT: f32 = 2.0;
const DRAG_PREVIEW_LIFT_Y: f32 = 6.0;
const DRAG_PREVIEW_SCALE: f32 = 1.02;
const DRAG_GAP_PULSE_EXTRA_ALPHA: f32 = 0.22;
const DRAG_GAP_BASE_ALPHA: f32 = 0.10;

// Minimum distance (in egui points) the cursor must travel outside the tab strip before we treat a
// drag as a "detach into new window" gesture.
pub(super) const TAB_DETACH_DRAG_THRESHOLD: f32 = 40.0;

const TAB_STRIP_SCROLL_CLAMP_DURATION: f32 = 0.16;

#[derive(Debug, Clone, Copy)]
struct TabStripScrollClampAnim {
  /// Whether the clamp animation is currently running.
  active: bool,
  start_offset_x: f32,
  target_offset_x: f32,
  /// The offset we last attempted to apply while clamping.
  ///
  /// Used to detect user-initiated scrolling during the clamp so we can restart the animation from
  /// the user's new position instead of "snapping back" to an outdated animation track.
  last_applied_offset_x: f32,
  start_time: f64,
  duration: f32,
}

impl Default for TabStripScrollClampAnim {
  fn default() -> Self {
    Self {
      active: false,
      start_offset_x: 0.0,
      target_offset_x: 0.0,
      last_applied_offset_x: 0.0,
      start_time: 0.0,
      duration: TAB_STRIP_SCROLL_CLAMP_DURATION,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabStripItemKey {
  GroupChip(TabGroupId),
  Tab(TabId),
}

#[derive(Debug, Clone)]
struct TabStripLayoutSnapshot {
  pinned_tabs: Vec<TabId>,
  unpinned_items: Vec<TabStripItemKey>,
  tab_rects: Vec<(TabId, Rect)>,
  unpinned_tab_width: f32,
  scroll_offset_x: f32,
  pinned_count: usize,
  unpinned_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct TabStripPerfSample {
  /// Time spent in `tab_strip_ui` up to snapshot persistence (microseconds).
  frame_us: u64,
  tab_count: usize,
  pinned_count: usize,
  unpinned_count: usize,
  pinned_tabs_cap: usize,
  unpinned_items_cap: usize,
  tab_rects_cap: usize,
}

fn tab_strip_perf_enabled() -> bool {
  static ENABLED: OnceLock<bool> = OnceLock::new();
  *ENABLED.get_or_init(|| std::env::var_os("FASTR_TAB_STRIP_PERF").is_some())
}

#[derive(Debug, Clone, Default)]
struct GroupChipWidthCache {
  /// Cache invalidation: group chip widths depend on the button font. If egui style changes the
  /// resolved `TextStyle::Button` font, recompute all cached widths.
  font_id: Option<FontId>,
  widths: FxHashMap<TabGroupId, GroupChipWidthCacheEntry>,
}

#[derive(Debug, Clone)]
struct GroupChipWidthCacheEntry {
  title: String,
  width: f32,
}

impl GroupChipWidthCache {
  fn width(&mut self, ui: &egui::Ui, group_id: TabGroupId, title: &str, font_id: &FontId) -> f32 {
    if self.font_id.as_ref() != Some(font_id) {
      self.font_id = Some(font_id.clone());
      self.widths.clear();
    }

    if let Some(entry) = self.widths.get(&group_id) {
      if entry.title == title {
        return entry.width;
      }
    }

    let width = group_chip_width_with_font(ui, title, font_id);
    self.widths.insert(
      group_id,
      GroupChipWidthCacheEntry {
        title: title.to_string(),
        width,
      },
    );
    width
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabPinAnimKind {
  Pin,
  Unpin,
}

#[derive(Debug, Clone)]
struct TabPinAnim {
  tab_id: TabId,
  kind: TabPinAnimKind,
  start_time: f64,
  duration: f32,
  from_rect: Rect,
  // For the source segment (where the tab is being removed), this is the insertion index at which
  // we add a shrinking placeholder to preserve old→new reflow.
  source_index: usize,
  // Snapshot of layout parameters from the frame before the pin/unpin state change.
  from_pinned_count: usize,
  from_unpinned_count: usize,
  from_unpinned_tab_width: f32,
}

#[derive(Debug, Clone, Copy)]
struct TabDragLiftOut {
  tab_id: TabId,
  /// Preview rect *before* lift/scale is applied (so lift-out can animate back to rest).
  base_rect: Rect,
}

fn tab_strip_scroll_clamp(
  ctx: &egui::Context,
  motion: UiMotion,
  clamp_anim_id: egui::Id,
  mut desired_scroll_offset_x: f32,
  max_scroll_x: f32,
  now: f64,
) -> (f32, f32, bool) {
  // Detect out-of-range offset after sizing is recomputed.
  if desired_scroll_offset_x > max_scroll_x + 0.5 {
    // Respect both reduced motion and egui's global animation toggle (Style::animation_time == 0).
    // When either disables animations, snap immediately (and avoid continuous repaint).
    let animations_enabled = motion.enabled && ctx.style().animation_time > 0.0;
    if animations_enabled {
      let mut anim = ctx
        .data(|d| d.get_temp::<TabStripScrollClampAnim>(clamp_anim_id))
        .unwrap_or_default();

      // If the user scrolled while we were clamping, restart the animation from the user's new
      // offset so we don't "snap back" to an outdated animation track.
      let user_scrolled = anim.active
        && anim.last_applied_offset_x.is_finite()
        && (desired_scroll_offset_x - anim.last_applied_offset_x).abs() > 0.5;
      if !anim.active || user_scrolled || (anim.target_offset_x - max_scroll_x).abs() > 0.5 {
        anim = TabStripScrollClampAnim {
          active: true,
          start_offset_x: desired_scroll_offset_x,
          target_offset_x: max_scroll_x,
          last_applied_offset_x: desired_scroll_offset_x,
          start_time: now,
          duration: TAB_STRIP_SCROLL_CLAMP_DURATION,
        };
      }

      let t = if anim.duration <= 0.0 {
        1.0
      } else {
        ((now - anim.start_time) as f32 / anim.duration).clamp(0.0, 1.0)
      };
      let t = ease_out_quad(t);
      desired_scroll_offset_x = lerp(anim.start_offset_x, anim.target_offset_x, t);
      anim.last_applied_offset_x = desired_scroll_offset_x;

      // Keep repainting until the scroll clamp finishes.
      if t < 1.0 - 1e-4 {
        ctx.request_repaint();
      } else {
        anim.active = false;
      }

      ctx.data_mut(|d| d.insert_temp(clamp_anim_id, anim));
    } else {
      desired_scroll_offset_x = max_scroll_x;
      ctx.data_mut(|d| d.insert_temp(clamp_anim_id, TabStripScrollClampAnim::default()));
    }

    // Ensure the ScrollArea accepts offsets beyond the new max by extending the content width with
    // a temporary spacer. As the desired offset animates down, the spacer shrinks so the strip
    // slides back smoothly instead of snapping.
    let end_spacer_x = (desired_scroll_offset_x - max_scroll_x).max(0.0);
    (desired_scroll_offset_x, end_spacer_x, true)
  } else {
    // Not clamping: ensure any previous clamp animation is inactive.
    ctx.data_mut(|d| d.insert_temp(clamp_anim_id, TabStripScrollClampAnim::default()));
    (desired_scroll_offset_x, 0.0, false)
  }
}

// Tab-strip drag auto-scroll parameters (when the unpinned segment overflows).
const DRAG_AUTOSCROLL_EDGE_ZONE_PX: f32 = 36.0;
const DRAG_AUTOSCROLL_MAX_SPEED_PX_PER_S: f32 = 1200.0;

// Treat very small collapse factors as fully collapsed to avoid leaving 1px slivers.
const GROUP_COLLAPSE_HIDE_EPS: f32 = 0.001;

fn drag_autoscroll_delta_x(pointer_pos: Pos2, viewport_rect: Rect, dt: f32) -> f32 {
  if dt <= 0.0 || !dt.is_finite() || viewport_rect.width() <= 0.0 {
    return 0.0;
  }

  // Don't let the activation zone exceed half the viewport width.
  let zone = DRAG_AUTOSCROLL_EDGE_ZONE_PX.min(viewport_rect.width() * 0.5);
  if zone <= 0.0 {
    return 0.0;
  }

  // Only auto-scroll while the pointer remains roughly aligned with the strip; this avoids
  // unexpected scrolling if the user drags far away (e.g. starting a detach gesture). Allow some
  // slack so dragging slightly above/below the strip still scrolls (common browser UX).
  let slack = TAB_DETACH_DRAG_THRESHOLD.max(zone);
  if pointer_pos.y < viewport_rect.top() - slack || pointer_pos.y > viewport_rect.bottom() + slack {
    return 0.0;
  }
  // Similarly, ignore pointer positions that are far outside the viewport horizontally (we only
  // want to auto-scroll when the user is near an edge).
  if pointer_pos.x < viewport_rect.left() - slack || pointer_pos.x > viewport_rect.right() + slack {
    return 0.0;
  }

  // Quadratic ramp: gentle when near the zone boundary, fast when hugging the edge.
  let mut delta_x = 0.0;
  if pointer_pos.x < viewport_rect.left() + zone {
    let t = ((viewport_rect.left() + zone) - pointer_pos.x) / zone;
    let t = t.clamp(0.0, 1.0);
    delta_x -= DRAG_AUTOSCROLL_MAX_SPEED_PX_PER_S * t * t * dt;
  } else if pointer_pos.x > viewport_rect.right() - zone {
    let t = (pointer_pos.x - (viewport_rect.right() - zone)) / zone;
    let t = t.clamp(0.0, 1.0);
    delta_x += DRAG_AUTOSCROLL_MAX_SPEED_PX_PER_S * t * t * dt;
  }

  delta_x
}

// When both pinned and unpinned tabs exist, keep the pinned segment from starving the unpinned
// scroll area (especially in narrow windows with many pinned tabs).
const PINNED_VIEWPORT_MAX_FRACTION: f32 = 0.45;
const MIN_UNPINNED_VIEWPORT: f32 = TAB_MIN_WIDTH + TAB_PADDING_X * 2.0;

fn tab_status_messages(tab: &BrowserTabState) -> (Option<&str>, Option<&str>) {
  let err = tab.error.as_deref().filter(|s| !s.trim().is_empty());
  let warn = tab.warning.as_deref().filter(|s| !s.trim().is_empty());
  (err, warn)
}

fn paint_tab_status_badges(
  painter: &egui::Painter,
  icon_rect: Rect,
  visuals: &egui::Visuals,
  error_t: f32,
  warning_t: f32,
) {
  if error_t <= 0.0 && warning_t <= 0.0 {
    return;
  }

  // Overlay small status dots on the favicon rect so error/warning states are discoverable even for
  // background tabs (without stealing horizontal space from titles).
  // Avoid per-frame allocations: this is called for every tab every frame.
  let mut colors = [Color32::TRANSPARENT; 2];
  let mut color_count = 0usize;
  if error_t > 0.0 {
    colors[color_count] = with_alpha(visuals.error_fg_color, error_t);
    color_count += 1;
  }
  if warning_t > 0.0 {
    colors[color_count] = with_alpha(visuals.warn_fg_color, warning_t);
    color_count += 1;
  }

  let radius = 3.0;
  let gap = 1.0;
  let inset = 1.0;
  let mut x = icon_rect.right() - radius - inset;
  let y = icon_rect.bottom() - radius - inset;
  let stroke_t = error_t.max(warning_t).clamp(0.0, 1.0);
  let stroke = Stroke::new(
    1.0,
    with_alpha(visuals.widgets.inactive.bg_stroke.color, stroke_t),
  );
  for &color in colors[..color_count].iter() {
    let center = Pos2::new(x, y);
    painter.circle_filled(center, radius, color);
    painter.circle_stroke(center, radius, stroke);
    x -= radius * 2.0 + gap;
  }
}

fn paint_tab_strip_edge_fade(painter: &egui::Painter, rect: Rect, color: Color32, left: bool) {
  let width = rect.width().max(0.0);
  if width <= 0.0 || rect.height() <= 0.0 {
    return;
  }
  // Approximate a horizontal gradient using a small stack of solid rectangles so we don't need
  // mesh/gradient primitives.
  let steps: usize = 10;
  let seg_w = (width / steps as f32).max(0.5);
  for i in 0..steps {
    let t = if steps <= 1 {
      1.0
    } else {
      i as f32 / ((steps - 1) as f32)
    };
    // Non-linear falloff so the fade is subtle near the inside edge.
    let alpha = (1.0 - t).powf(2.2);
    let fill = with_alpha(color, alpha);
    let x0 = if left {
      rect.left() + (i as f32) * seg_w
    } else {
      rect.right() - ((i + 1) as f32) * seg_w
    };
    let x1 = x0 + seg_w;
    let seg = Rect::from_min_max(Pos2::new(x0, rect.top()), Pos2::new(x1, rect.bottom()));
    painter.rect_filled(seg, 0.0, fill);
  }
}

fn paint_scroll_edge_fades(
  ui: &egui::Ui,
  viewport_rect: Rect,
  scroll_offset_x: f32,
  max_scroll_x: f32,
) {
  if viewport_rect.width() <= 0.0 || viewport_rect.height() <= 0.0 || max_scroll_x <= 0.0 {
    return;
  }

  let fade_w = 18.0_f32.min(viewport_rect.width() * 0.5);
  if fade_w <= 0.0 {
    return;
  }

  // Ramp the fade alpha in/out smoothly based on how close we are to the edge so it doesn't pop.
  let left_t = (scroll_offset_x / fade_w).clamp(0.0, 1.0);
  let right_t = ((max_scroll_x - scroll_offset_x) / fade_w).clamp(0.0, 1.0);

  if left_t <= 0.0 && right_t <= 0.0 {
    return;
  }

  let fade_rect = Rect::from_min_max(
    viewport_rect.min,
    Pos2::new(viewport_rect.max.x, viewport_rect.max.y - 1.0),
  );
  let painter = ui.painter().with_clip_rect(viewport_rect);
  let fade_color = ui.visuals().panel_fill;

  if left_t > 0.0 {
    let left_rect = Rect::from_min_max(
      fade_rect.min,
      Pos2::new(fade_rect.min.x + fade_w, fade_rect.max.y),
    );
    paint_tab_strip_edge_fade(&painter, left_rect, with_alpha(fade_color, left_t), true);
  }
  if right_t > 0.0 {
    let right_rect = Rect::from_min_max(
      Pos2::new(fade_rect.max.x - fade_w, fade_rect.min.y),
      fade_rect.max,
    );
    paint_tab_strip_edge_fade(&painter, right_rect, with_alpha(fade_color, right_t), false);
  }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
  a + (b - a) * t
}

fn ease_out_quad(t: f32) -> f32 {
  let t = t.clamp(0.0, 1.0);
  1.0 - (1.0 - t) * (1.0 - t)
}

fn lerp_pos(a: Pos2, b: Pos2, t: f32) -> Pos2 {
  Pos2::new(lerp(a.x, b.x, t), lerp(a.y, b.y, t))
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
  Rect::from_min_max(lerp_pos(a.min, b.min, t), lerp_pos(a.max, b.max, t))
}

fn ease_in_out_cubic(t: f32) -> f32 {
  let t = t.clamp(0.0, 1.0);
  if t < 0.5 {
    4.0 * t * t * t
  } else {
    1.0 - (-2.0 * t + 2.0).powf(3.0) * 0.5
  }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
  lerp(a as f32, b as f32, t).round().clamp(0.0, 255.0) as u8
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
  let [ar, ag, ab, aa] = a.to_array();
  let [br, bg, bb, ba] = b.to_array();
  egui::Color32::from_rgba_unmultiplied(
    lerp_u8(ar, br, t),
    lerp_u8(ag, bg, t),
    lerp_u8(ab, bb, t),
    lerp_u8(aa, ba, t),
  )
}

fn with_alpha(color: egui::Color32, alpha: f32) -> egui::Color32 {
  let [r, g, b, a] = color.to_array();
  let a = ((a as f32) * alpha).round().clamp(0.0, 255.0) as u8;
  egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn contrast_bw(bg: egui::Color32) -> egui::Color32 {
  // Simple sRGB luma heuristic. Good enough for choosing a contrasting "halo" color.
  let r = bg.r() as f32 / 255.0;
  let g = bg.g() as f32 / 255.0;
  let b = bg.b() as f32 / 255.0;
  let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  if luma > 0.5 {
    egui::Color32::BLACK
  } else {
    egui::Color32::WHITE
  }
}

fn group_color_egui(color: TabGroupColor) -> Color32 {
  let (r, g, b) = color.rgb();
  Color32::from_rgb(r, g, b)
}

fn group_color_fill(color: TabGroupColor) -> Color32 {
  let (r, g, b) = color.rgb();
  Color32::from_rgba_unmultiplied(r, g, b, 48)
}

fn compute_tab_insertion_index(
  pointer_x: f32,
  tab_rects: &[(TabId, Rect)],
  dragged_id: TabId,
) -> usize {
  if pointer_x.is_nan() || pointer_x == f32::NEG_INFINITY {
    return 0;
  }
  if pointer_x == f32::INFINITY {
    return tab_rects
      .iter()
      .filter(|(tab_id, _)| *tab_id != dragged_id)
      .count();
  }

  // Compare against tab centers (ignoring the dragged tab itself). We intentionally treat equality
  // as being on the "after" side so the boundary is deterministic.
  let mut insertion_index: usize = 0;
  for (tab_id, rect) in tab_rects {
    if *tab_id == dragged_id {
      continue;
    }
    if pointer_x < rect.center().x {
      break;
    }
    insertion_index += 1;
  }
  insertion_index
}

fn paint_popup_shadow(
  painter: &egui::Painter,
  rect: Rect,
  rounding: egui::Rounding,
  shadow: egui::epaint::Shadow,
) {
  if shadow.extrusion <= 0.0 || shadow.color.a() == 0 {
    return;
  }

  // Egui's `Shadow` type doesn't currently have a simple paint helper exposed, so approximate a
  // blurred drop shadow using a small stack of expanded translucent rects.
  let steps: usize = 6;
  let max_expand = shadow.extrusion.max(0.0);
  // Slight downward bias so the tab reads as "lifted" above the strip.
  let offset = Vec2::new(0.0, max_expand * 0.25);
  for i in 0..steps {
    let t = (i + 1) as f32 / (steps as f32);
    let expand = max_expand * t;
    // Stronger near the center, softer further out.
    let alpha = (1.0 - t).powf(2.0);
    let color = with_alpha(shadow.color, alpha);
    painter.rect_filled(rect.expand(expand).translate(offset), rounding, color);
  }
}

fn paint_drag_placeholder(
  painter: &egui::Painter,
  rect: Rect,
  visuals: &egui::Visuals,
  group_color: Option<Color32>,
  pulse_t: f32,
) {
  let rounding = visuals.widgets.inactive.rounding;
  let stroke_color = group_color.unwrap_or(visuals.selection.stroke.color);
  let fill_alpha = (DRAG_GAP_BASE_ALPHA + DRAG_GAP_PULSE_EXTRA_ALPHA * pulse_t).clamp(0.0, 1.0);
  let stroke_alpha = (0.25 + 0.35 * pulse_t).clamp(0.0, 1.0);

  // Keep the physical gap stable, but animate the inner highlight width a bit so it feels like the
  // insertion slot is "breathing" as it moves.
  let base_w = rect.width() * 0.88;
  let width = lerp(base_w, rect.width(), pulse_t.clamp(0.0, 1.0));
  let inner = Rect::from_center_size(rect.center(), Vec2::new(width, rect.height()));

  painter.rect_filled(
    inner.shrink(1.0),
    rounding,
    with_alpha(
      visuals.widgets.inactive.bg_fill.gamma_multiply(0.9),
      fill_alpha,
    ),
  );
  painter.rect_stroke(
    inner.shrink(0.5),
    rounding,
    Stroke::new(1.0, with_alpha(stroke_color, stroke_alpha)),
  );
}

fn unpinned_tab_preview_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  tab: &BrowserTabState,
  is_active: bool,
  can_close_tabs: bool,
  tab_rect: Rect,
  favicon_tex: Option<egui::TextureId>,
  group_color: Option<Color32>,
) {
  let tab_id = tab_strip_tab_widget_id(tab.id);
  let title = tab.display_title();
  let (has_error, has_warning) = {
    let (err, warn) = tab_status_messages(tab);
    (err.is_some(), warn.is_some())
  };
  let visuals = ui.style().visuals.clone();

  // Draw the dragged tab slightly "hovered" so it reads as lifted.
  let hover_t = if is_active { 0.0 } else { 1.0 };
  let bg = if is_active {
    visuals.widgets.active.bg_fill
  } else {
    lerp_color(
      visuals.widgets.inactive.bg_fill,
      visuals.widgets.hovered.bg_fill,
      hover_t,
    )
  };
  let rounding = visuals.widgets.inactive.rounding;

  {
    let painter = ui.painter();
    painter.rect_filled(tab_rect, rounding, bg);
    if let Some(color) = group_color {
      painter.rect_stroke(tab_rect.shrink(0.5), rounding, Stroke::new(1.0, color));
    }
  }

  // Favicon.
  let icon_min = Pos2::new(
    tab_rect.min.x + TAB_PADDING_X,
    tab_rect.center().y - ICON_SIZE * 0.5,
  );
  let icon_rect = Rect::from_min_size(icon_min, Vec2::splat(ICON_SIZE));
  if let Some(tex_id) = favicon_tex {
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    ui.painter().image(tex_id, icon_rect, uv, Color32::WHITE);
  } else {
    placeholder_favicon(ui.painter(), icon_rect, &visuals, 1.0);
    let mut glyph_buf = [0u8; 4];
    let glyph = title
      .trim()
      .chars()
      .next()
      .unwrap_or('?')
      .to_ascii_uppercase()
      .encode_utf8(&mut glyph_buf);
    ui.painter().text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(12.0),
      with_alpha(visuals.text_color(), 0.75),
    );
  }

  if tab.loading {
    // Spinner overlay around the favicon.
    let time = if motion.enabled && ui.ctx().style().animation_time > 0.0 {
      ui.ctx().request_repaint();
      ui.input(|i| i.time)
    } else {
      // Reduced-motion: keep the spinner static (and avoid continuous repaints).
      0.0
    };
    paint_spinner(
      ui.painter(),
      icon_rect.expand(2.0),
      time,
      visuals.text_color(),
    );
  }

  let err_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_error"),
    has_error,
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    has_warning,
    motion.durations.progress_fade,
  );
  paint_tab_status_badges(ui.painter(), icon_rect, &visuals, err_t, warn_t);

  // Close icon (non-interactive in the drag preview).
  let close_rect = if can_close_tabs {
    let close_min = Pos2::new(
      tab_rect.max.x - TAB_PADDING_X - CLOSE_BUTTON_SIZE,
      tab_rect.center().y - CLOSE_BUTTON_SIZE * 0.5,
    );
    Some(Rect::from_min_size(
      close_min,
      Vec2::splat(CLOSE_BUTTON_SIZE),
    ))
  } else {
    None
  };
  if let Some(close_rect) = close_rect {
    paint_icon_in_rect(
      ui,
      close_rect,
      BrowserIcon::CloseTab,
      ICON_SIZE,
      with_alpha(visuals.text_color(), 0.85),
    );
  }

  // Title.
  let title_start_x = icon_rect.max.x + ICON_GAP;
  let title_end_x = close_rect
    .map(|r| r.min.x - ICON_GAP)
    .unwrap_or(tab_rect.max.x - TAB_PADDING_X);
  if title_end_x > title_start_x + 4.0 {
    let title_rect = Rect::from_min_max(
      Pos2::new(title_start_x, tab_rect.min.y),
      Pos2::new(title_end_x, tab_rect.max.y),
    );
    let label = {
      let mut text = egui::RichText::new(title);
      if is_active {
        text = text.strong();
      }
      egui::Label::new(text).truncate(true).wrap(false)
    };
    let _ = ui.put(title_rect, label);
  }
}

fn pinned_tab_preview_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  tab: &BrowserTabState,
  is_active: bool,
  tab_rect: Rect,
  favicon_tex: Option<egui::TextureId>,
) {
  let tab_id = tab_strip_tab_widget_id(tab.id);
  let title = tab.display_title();
  let (has_error, has_warning) = {
    let (err, warn) = tab_status_messages(tab);
    (err.is_some(), warn.is_some())
  };
  let visuals = ui.style().visuals.clone();

  // Pinned tabs are icon-only; keep the same visual styling, but treat as hovered so it feels
  // lifted.
  let hover_t = if is_active { 0.0 } else { 1.0 };
  let bg = if is_active {
    visuals.widgets.active.bg_fill
  } else {
    lerp_color(
      visuals.widgets.inactive.bg_fill,
      visuals.widgets.hovered.bg_fill,
      hover_t,
    )
  };
  let rounding = visuals.widgets.inactive.rounding;
  ui.painter().rect_filled(tab_rect, rounding, bg);

  // Favicon (centered).
  let icon_min = Pos2::new(
    tab_rect.center().x - ICON_SIZE * 0.5,
    tab_rect.center().y - ICON_SIZE * 0.5,
  );
  let icon_rect = Rect::from_min_size(icon_min, Vec2::splat(ICON_SIZE));
  if let Some(tex_id) = favicon_tex {
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    ui.painter().image(tex_id, icon_rect, uv, Color32::WHITE);
  } else {
    placeholder_favicon(ui.painter(), icon_rect, &visuals, 1.0);
    let mut glyph_buf = [0u8; 4];
    let glyph = title
      .trim()
      .chars()
      .next()
      .unwrap_or('?')
      .to_ascii_uppercase()
      .encode_utf8(&mut glyph_buf);
    ui.painter().text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(14.0),
      visuals.text_color(),
    );
  }

  if tab.loading {
    let time = if motion.enabled && ui.ctx().style().animation_time > 0.0 {
      ui.ctx().request_repaint();
      ui.input(|i| i.time)
    } else {
      0.0
    };
    paint_spinner(
      ui.painter(),
      icon_rect.expand(2.0),
      time,
      visuals.text_color(),
    );
  }

  let err_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_error"),
    has_error,
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    has_warning,
    motion.durations.progress_fade,
  );
  paint_tab_status_badges(ui.painter(), icon_rect, &visuals, err_t, warn_t);
}

fn paint_tab_pin_ghost(
  ui: &egui::Ui,
  motion: UiMotion,
  tab: &BrowserTabState,
  rect: Rect,
  favicon_tex: Option<egui::TextureId>,
  is_active: bool,
  kind: TabPinAnimKind,
  t: f32,
) {
  if rect.width() <= 0.0 || rect.height() <= 0.0 {
    return;
  }

  // The ghost is purely visual: swallow pointer interactions so clicks/drags on the moving tab
  // don't accidentally hit whatever happens to be under it mid-flight.
  let blocker_id = ui.make_persistent_id(("tab_strip_pin_ghost_blocker", tab.id));
  let _ = ui
    .interact(rect, blocker_id, Sense::click_and_drag())
    .on_hover_cursor(egui::CursorIcon::Default);

  let visuals = ui.style().visuals.clone();
  let bg = if is_active {
    visuals.widgets.active.bg_fill
  } else {
    visuals.widgets.inactive.bg_fill
  };
  let rounding = visuals.widgets.inactive.rounding;
  let painter = ui.painter().with_clip_rect(rect);
  painter.rect_filled(rect, rounding, bg);

  // Let the favicon migrate from "unpinned" (left aligned) → "pinned" (centered) as the tab
  // transitions, so the motion feels like a single morph rather than a hard style swap.
  let icon_t = match kind {
    TabPinAnimKind::Pin => t,
    TabPinAnimKind::Unpin => 1.0 - t,
  };
  let unpinned_center_x = rect.min.x + TAB_PADDING_X + ICON_SIZE * 0.5;
  let pinned_center_x = rect.center().x;
  let icon_center_x = lerp(unpinned_center_x, pinned_center_x, icon_t);
  let icon_center = Pos2::new(icon_center_x, rect.center().y);
  let icon_rect = Rect::from_center_size(icon_center, Vec2::splat(ICON_SIZE));

  if let Some(tex_id) = favicon_tex {
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    painter.image(tex_id, icon_rect, uv, Color32::WHITE);
  } else {
    placeholder_favicon(&painter, icon_rect, &visuals, 1.0);
    let title = tab.display_title();
    let mut glyph_buf = [0u8; 4];
    let glyph = title
      .trim()
      .chars()
      .next()
      .unwrap_or('?')
      .to_ascii_uppercase()
      .encode_utf8(&mut glyph_buf);
    painter.text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(12.0),
      with_alpha(visuals.text_color(), 0.75),
    );
  }

  if tab.loading {
    let time = if motion.enabled && ui.ctx().style().animation_time > 0.0 {
      ui.ctx().request_repaint();
      ui.input(|i| i.time)
    } else {
      0.0
    };
    paint_spinner(&painter, icon_rect.expand(2.0), time, visuals.text_color());
  }

  let (err, warn) = tab_status_messages(tab);
  paint_tab_status_badges(
    &painter,
    icon_rect,
    &visuals,
    if err.is_some() { 1.0 } else { 0.0 },
    if warn.is_some() { 1.0 } else { 0.0 },
  );

  // Title (clipped). We intentionally omit the close button so the moving tab doesn't invite
  // interaction mid-flight.
  let title = tab.display_title();
  let title_start_x = rect.min.x + TAB_PADDING_X + ICON_SIZE + ICON_GAP;
  if rect.width() > PINNED_TAB_WIDTH + 16.0 && title_start_x < rect.max.x - 4.0 {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    painter.text(
      Pos2::new(title_start_x, rect.center().y),
      Align2::LEFT_CENTER,
      title,
      font_id,
      visuals.text_color(),
    );
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TabStripSizing {
  pub tab_width: f32,
  pub overflow: bool,
  pub total_content_width: f32,
}

#[derive(Debug)]
enum TabStripOp {
  ToggleGroupCollapsed(TabGroupId),
  Ungroup(TabGroupId),
  SetGroupColor(TabGroupId, TabGroupColor),
}

/// Pure sizing logic for the tab strip.
///
/// This keeps the strip single-row by shrinking tabs down to `TAB_MIN_WIDTH`, and reports whether
/// the strip will overflow the available viewport width at that minimum.
///
/// The strip can also contain fixed-width "extra" items (e.g. tab group chips) which participate in
/// the same spacing row as normal tabs.
///
/// `gap_count` is the number of inter-item gaps (of width `TAB_GAP`) between all items (tabs +
/// fixed-width extras) in the unpinned strip.
pub(super) fn compute_tab_strip_sizing_with_fixed_width(
  available_width: f32,
  tab_count: usize,
  fixed_items_total_width: f32,
  gap_count: usize,
) -> TabStripSizing {
  let available_width = if available_width.is_finite() {
    available_width.max(0.0)
  } else {
    0.0
  };

  let fixed_items_total_width = if fixed_items_total_width.is_finite() {
    fixed_items_total_width.max(0.0)
  } else {
    0.0
  };

  let gaps = {
    let v = TAB_GAP * (gap_count as f32);
    if v.is_finite() {
      v.max(0.0)
    } else {
      0.0
    }
  };

  let tab_width = if tab_count == 0 {
    0.0
  } else {
    let available_for_tabs = (available_width - gaps - fixed_items_total_width).max(0.0);
    let ideal_width = (available_for_tabs / (tab_count as f32)).max(0.0);
    ideal_width.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
  };

  let total_content_width = fixed_items_total_width + (tab_width * tab_count as f32) + gaps;
  let overflow = total_content_width > available_width + 0.01;

  TabStripSizing {
    tab_width,
    overflow,
    total_content_width,
  }
}

/// Sizing helper for the unpinned tab strip when some "tabs" are partially collapsed.
///
/// `tab_units` represents the effective number of full-width tabs. For example, a tab rendered at
/// `tab_width * 0.5` contributes `0.5` units.
///
/// `total_gap_width` is the total *actual* spacing between rendered items (after any scaling),
/// rather than assuming a fixed `TAB_GAP * (item_count - 1)`.
fn compute_tab_strip_sizing_with_scaled_tabs(
  available_width: f32,
  tab_units: f32,
  extra_item_width: f32,
  total_gap_width: f32,
) -> TabStripSizing {
  let available_width = if available_width.is_finite() {
    available_width.max(0.0)
  } else {
    0.0
  };
  let extra_item_width = if extra_item_width.is_finite() {
    extra_item_width.max(0.0)
  } else {
    0.0
  };
  let total_gap_width = if total_gap_width.is_finite() {
    total_gap_width.max(0.0)
  } else {
    0.0
  };
  let tab_units = if tab_units.is_finite() {
    tab_units.max(0.0)
  } else {
    0.0
  };

  let tab_width = if tab_units <= 0.0 {
    0.0
  } else {
    let available_for_tabs = (available_width - total_gap_width - extra_item_width).max(0.0);
    let ideal_width = (available_for_tabs / tab_units).max(0.0);
    ideal_width.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
  };

  let total_content_width = extra_item_width + total_gap_width + tab_width * tab_units;
  let overflow = total_content_width > available_width + 0.01;

  TabStripSizing {
    tab_width,
    overflow,
    total_content_width,
  }
}

pub(super) fn compute_tab_strip_sizing(available_width: f32, tab_count: usize) -> TabStripSizing {
  compute_tab_strip_sizing_with_fixed_width(
    available_width,
    tab_count,
    0.0,
    tab_count.saturating_sub(1),
  )
}

/// Compute how much horizontal space to allocate to pinned vs unpinned tab viewports.
///
/// This is pure sizing logic so it can be unit-tested. When pinned tabs overflow, they should
/// scroll within their own region instead of consuming the full strip width and pushing the
/// unpinned region to zero.
pub(super) fn compute_pinned_viewport_width(
  total_tabs_width: f32,
  pinned_content_width: f32,
  has_unpinned_tabs: bool,
) -> (f32, f32) {
  let total_tabs_width = if total_tabs_width.is_finite() {
    total_tabs_width.max(0.0)
  } else {
    0.0
  };
  let pinned_content_width = if pinned_content_width.is_finite() {
    pinned_content_width.max(0.0)
  } else {
    0.0
  };

  if pinned_content_width <= 0.0 {
    return (0.0, total_tabs_width);
  }
  if !has_unpinned_tabs {
    // When only pinned tabs exist, give them the full strip width so overflow can still scroll.
    return (total_tabs_width, 0.0);
  }

  fn pinned_width_for_constraints(total: f32, pinned_content: f32, segment_gap: f32) -> f32 {
    let max_by_fraction = total * PINNED_VIEWPORT_MAX_FRACTION;
    let max_by_unpinned = (total - MIN_UNPINNED_VIEWPORT - segment_gap).max(0.0);
    pinned_content
      .min(max_by_fraction.min(max_by_unpinned))
      .max(PINNED_TAB_WIDTH.min(total))
      .min(total)
  }

  // Default to showing a gap between pinned + unpinned segments, but drop it if doing so would
  // collapse one of the segments entirely under very narrow widths.
  let mut segment_gap = TAB_GAP;
  let mut pinned_viewport_width =
    pinned_width_for_constraints(total_tabs_width, pinned_content_width, segment_gap);
  let mut unpinned_viewport_width =
    (total_tabs_width - pinned_viewport_width - segment_gap).max(0.0);
  if unpinned_viewport_width <= 0.0 && segment_gap > 0.0 {
    segment_gap = 0.0;
    pinned_viewport_width =
      pinned_width_for_constraints(total_tabs_width, pinned_content_width, segment_gap);
    unpinned_viewport_width = (total_tabs_width - pinned_viewport_width - segment_gap).max(0.0);
  }

  let pinned_viewport_width = pinned_viewport_width.clamp(0.0, total_tabs_width);
  let unpinned_viewport_width =
    unpinned_viewport_width.clamp(0.0, (total_tabs_width - pinned_viewport_width).max(0.0));

  (pinned_viewport_width, unpinned_viewport_width)
}

fn sanitize_tab_width(width: f32) -> f32 {
  if width.is_finite() {
    width.max(0.0)
  } else {
    0.0
  }
}

fn paint_spinner(painter: &egui::Painter, rect: Rect, time: f64, color: Color32) {
  let center = rect.center();
  let radius = rect.width().min(rect.height()) * 0.5 - 1.0;
  if radius <= 0.0 {
    return;
  }

  // 12 spoke spinner.
  let n = 12;
  let angle = (time as f32) * 6.0; // ~1 rotation per second.
  let base_alpha = (color.a() as f32 / 255.0).clamp(0.0, 1.0);
  for i in 0..n {
    let t = i as f32 / n as f32;
    // Newest segment is brightest.
    let alpha = (255.0 * base_alpha * (1.0 - t).powf(2.0))
      .round()
      .clamp(0.0, 255.0) as u8;
    let a = angle + t * std::f32::consts::TAU;
    let dir = egui::vec2(a.cos(), a.sin());
    let start = center + dir * (radius * 0.55);
    let end = center + dir * radius;
    painter.line_segment(
      [start, end],
      Stroke::new(
        2.0,
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
      ),
    );
  }
}

fn placeholder_favicon(painter: &egui::Painter, rect: Rect, visuals: &egui::Visuals, alpha: f32) {
  let fill = with_alpha(visuals.widgets.inactive.bg_fill, alpha);
  let mut stroke = visuals.widgets.inactive.bg_stroke;
  stroke.color = with_alpha(stroke.color, alpha);
  // Keep favicon placeholders subtly rounded without looking fully pill-shaped.
  let rounding = egui::Rounding::same((visuals.widgets.inactive.rounding.nw * 0.5).clamp(2.0, 4.0));
  painter.rect_filled(rect, rounding, fill);
  painter.rect_stroke(rect, rounding, stroke);
}

fn group_chip_width_with_font(ui: &egui::Ui, label: &str, font_id: &FontId) -> f32 {
  let galley = ui.fonts(|f| {
    f.layout_no_wrap(
      label.to_string(),
      font_id.clone(),
      ui.visuals().text_color(),
    )
  });
  // Reserve fixed space for the collapse/expand affordance icon so the chip width remains stable
  // across collapsed/expanded states.
  (galley.size().x + GROUP_CHIP_PADDING_X * 2.0 + GROUP_CHIP_ICON_SIZE + GROUP_CHIP_ICON_GAP)
    .clamp(GROUP_CHIP_MIN_WIDTH, GROUP_CHIP_MAX_WIDTH)
}

fn group_chip_width(ui: &egui::Ui, label: &str) -> f32 {
  let font_id = egui::TextStyle::Button.resolve(ui.style());
  group_chip_width_with_font(ui, label, &font_id)
}

fn group_chip_title(group: &TabGroupState) -> &str {
  if group.title.trim().is_empty() {
    "Group"
  } else {
    group.title.as_str()
  }
}

fn group_chip_a11y_label(title: &str, collapsed: bool) -> String {
  if collapsed {
    format!("Expand tab group: {title}")
  } else {
    format!("Collapse tab group: {title}")
  }
}

fn group_color_menu_item_a11y_label(color: TabGroupColor) -> &'static str {
  match color {
    TabGroupColor::Blue => "Set group color: Blue",
    TabGroupColor::Gray => "Set group color: Gray",
    TabGroupColor::Red => "Set group color: Red",
    TabGroupColor::Orange => "Set group color: Orange",
    TabGroupColor::Yellow => "Set group color: Yellow",
    TabGroupColor::Green => "Set group color: Green",
    TabGroupColor::Purple => "Set group color: Purple",
    TabGroupColor::Pink => "Set group color: Pink",
  }
}

#[derive(Debug, Clone, Copy)]
struct GroupChipContextMenuState {
  open: bool,
  /// Screen-space anchor position in egui points.
  anchor_pos: Pos2,
}

impl Default for GroupChipContextMenuState {
  fn default() -> Self {
    Self {
      open: false,
      anchor_pos: Pos2::ZERO,
    }
  }
}

fn group_chip_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  app: &mut BrowserAppState,
  group_id: TabGroupId,
  ops: &mut Vec<TabStripOp>,
  focus_ring: FocusRingStyle,
  precomputed_width: Option<f32>,
) {
  let id = ui.make_persistent_id(("tab_group_chip", group_id.0));
  let (collapsed, mut response, chip_rect) = {
    let Some(group) = app.tab_groups.get_mut(&group_id) else {
      return;
    };
    let color = group.color;
    let collapsed = group.collapsed;
    let a11y_label = group.tab_group_chip_accessible_label();
    let title = group_chip_title(group);

    let width = precomputed_width
      .filter(|w| w.is_finite())
      .map(|w| w.max(0.0).clamp(GROUP_CHIP_MIN_WIDTH, GROUP_CHIP_MAX_WIDTH))
      .unwrap_or_else(|| group_chip_width(ui, title));
    let (_, chip_rect) = ui.allocate_space(Vec2::new(width, TAB_HEIGHT));
    let mut response = ui.interact(chip_rect, id, Sense::click());
    if response.hovered() {
      response = response.on_hover_text(title);
    }
    response.widget_info({
      let a11y_label = a11y_label;
      move || egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label.as_ref())
    });
    // Accessibility: if focus lands on a chip that is outside the horizontal scroll viewport, bring
    // it into view so keyboard and screen-reader users can see the focused control.
    //
    // Guard against repaint loops by only scrolling when the rect is actually not visible.
    if response.has_focus() && !ui.is_rect_visible(chip_rect) {
      response.scroll_to_me(Some(egui::Align::Center));
    }

    #[cfg(test)]
    {
      ui.ctx().data_mut(|d| {
        d.insert_temp(egui::Id::new("test_tab_group_chip_id"), response.id);
        d.insert_temp(egui::Id::new("test_tab_group_chip_rect"), chip_rect);
      });
    }

    let visuals = ui.style().visuals.clone();

    // Micro-interaction: fade hover highlight in/out (keeping group color identity).
    let hover_t = motion.animate_bool(
      ui.ctx(),
      id.with("hover"),
      response.hovered(),
      motion.durations.hover_fade,
    );
    let pressed = ui.is_enabled() && response.is_pointer_button_down_on();

    let (r, g, b) = color.rgb();
    let fill_base = Color32::from_rgba_unmultiplied(r, g, b, 48);
    let fill_hover = Color32::from_rgba_unmultiplied(r, g, b, 72);
    let fill_active = Color32::from_rgba_unmultiplied(r, g, b, 92);
    let mut fill = lerp_color(fill_base, fill_hover, hover_t);
    if pressed {
      fill = fill_active;
    }

    let stroke_base = with_alpha(group_color_egui(color), 0.85);
    let stroke_hover = group_color_egui(color);
    let stroke_color = if pressed {
      stroke_hover
    } else {
      lerp_color(stroke_base, stroke_hover, hover_t)
    };
    let stroke = Stroke::new(1.0, stroke_color);

    let rounding = visuals.widgets.inactive.rounding;
    ui.painter().rect_filled(chip_rect, rounding, fill);
    ui.painter()
      .rect_stroke(chip_rect.shrink(0.5), rounding, stroke);

    // Collapse/expand indicator: paint a small rotating triangle instead of swapping text glyphs.
    let expanded_t = motion.animate_bool(
      ui.ctx(),
      id.with("expanded"),
      !collapsed,
      motion.durations.tab_group_collapse,
    );
    let angle = expanded_t * std::f32::consts::FRAC_PI_2;

    let icon_min = Pos2::new(
      chip_rect.min.x + GROUP_CHIP_PADDING_X,
      chip_rect.center().y - GROUP_CHIP_ICON_SIZE * 0.5,
    );
    let icon_rect = Rect::from_min_size(icon_min, Vec2::splat(GROUP_CHIP_ICON_SIZE));
    let icon_center = icon_rect.center();
    let (sin, cos) = angle.sin_cos();
    let rot = |v: Vec2| Vec2::new(v.x * cos - v.y * sin, v.x * sin + v.y * cos);
    let half = GROUP_CHIP_ICON_SIZE * 0.5;
    let tri = [
      Vec2::new(-half * 0.35, -half * 0.55),
      Vec2::new(-half * 0.35, half * 0.55),
      Vec2::new(half * 0.55, 0.0),
    ];
    let points: Vec<Pos2> = tri.iter().map(|v| icon_center + rot(*v)).collect();
    ui.painter().add(egui::Shape::convex_polygon(
      points,
      visuals.text_color(),
      Stroke::NONE,
    ));

    // Title.
    let title_start_x = icon_rect.max.x + GROUP_CHIP_ICON_GAP;
    let title_end_x = chip_rect.max.x - GROUP_CHIP_PADDING_X;
  if title_end_x > title_start_x + 4.0 {
    let title_rect = Rect::from_min_max(
      Pos2::new(title_start_x, chip_rect.min.y),
      Pos2::new(title_end_x, chip_rect.max.y),
    );
    if ui.is_rect_visible(title_rect) {
      let label = egui::Label::new(egui::RichText::new(title).text_style(egui::TextStyle::Button))
        .truncate(true)
        .wrap(false);
      let _ = ui.put(title_rect, label);
    }
  }

    (collapsed, response, chip_rect)
  };

  let menu_state_id = id.with("context_menu_state");
  let mut menu_state = ui
    .ctx()
    .data(|d| d.get_temp::<GroupChipContextMenuState>(menu_state_id))
    .unwrap_or_default();
  let menu_open_prev = menu_state.open;

  let mut chip_activated = response.clicked();
  chip_activated |= super::keyboard_activate(ui, &response);
  if chip_activated {
    ops.push(TabStripOp::ToggleGroupCollapsed(group_id));
    // Clicking the chip while its context menu is open should dismiss the menu (standard popup
    // behaviour).
    menu_state.open = false;
  }

  // Open the context menu with either:
  // - Pointer: right click on the chip (existing behaviour)
  // - Keyboard: Shift+F10 while the chip has focus (Windows-style context menu key gesture)
  //
  // Egui's built-in `Response::context_menu` does not currently provide a keyboard activation path,
  // so we manage open-state explicitly.
  let open_by_pointer = response.clicked_by(egui::PointerButton::Secondary);
  let open_by_keyboard = response.has_focus()
    && ui.input_mut(|i| {
      i.consume_key(
        egui::Modifiers {
          shift: true,
          ..Default::default()
        },
        egui::Key::F10,
      )
    });

  let mut opened_now_via_keyboard = false;
  if open_by_pointer {
    // Anchor to the click position (or hover position) so the menu appears where the user clicked.
    if let Some(pos) = response
      .interact_pointer_pos()
      .or_else(|| ui.input(|i| i.pointer.hover_pos()))
    {
      menu_state.anchor_pos = pos;
    } else {
      menu_state.anchor_pos = Pos2::new(chip_rect.left(), chip_rect.bottom());
    }
    menu_state.open = true;
  } else if open_by_keyboard {
    if menu_state.open {
      // Pressing Shift+F10 again closes the menu (mirrors typical context menu toggle behaviour).
      menu_state.open = false;
    } else {
      // Anchor below the chip when opened via keyboard (no cursor position).
      menu_state.anchor_pos = Pos2::new(chip_rect.left(), chip_rect.bottom());
      menu_state.open = true;
      opened_now_via_keyboard = true;
    }
  }

  let mut menu_rect: Option<Rect> = None;
  if menu_state.open {
    let mut close_menu = false;
    // Escape should dismiss the menu when it's open.
    if ui.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape)) {
      close_menu = true;
    }

    let menu_id = id.with("context_menu_popup");
    let area = egui::Area::new(menu_id)
      .order(egui::Order::Foreground)
      .fixed_pos(menu_state.anchor_pos)
      .constrain_to(ui.ctx().screen_rect())
      .interactable(true);
    let inner = area.show(ui.ctx(), |ui| {
      let frame = egui::Frame::popup(ui.style());
      frame
        .show(ui, |ui| {
          ui.set_min_width(220.0);

          ui.label("Rename group");
          let rename_id = ui.make_persistent_id(("tab_group_rename", group_id.0));
          let mut new_title = app
            .tab_groups
            .get(&group_id)
            .map(|g| g.title.clone())
            .unwrap_or_default();
          let rename = ui.add(
            egui::TextEdit::singleline(&mut new_title)
              .id(rename_id)
              .hint_text("Tab group name"),
          );
          rename.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Tab group name")
          });
          if opened_now_via_keyboard {
            rename.request_focus();
          }
          if rename.changed() {
            app.set_group_title(group_id, new_title);
          }

          #[cfg(test)]
          ui.ctx().data_mut(|d| {
            d.insert_temp(egui::Id::new("test_tab_group_rename_id"), rename_id);
          });

          ui.separator();

          let change_color_menu = ui.menu_button("Change color", |ui| {
            for color in TabGroupColor::ALL {
              let button = egui::Button::new(color.as_str())
                .fill(group_color_fill(color))
                .stroke(Stroke::new(1.0, group_color_egui(color)));
              let resp = ui.add(button);
              resp.widget_info({
                move || {
                  egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    group_color_menu_item_a11y_label(color),
                  )
                }
              });
              if resp.clicked() {
                ops.push(TabStripOp::SetGroupColor(group_id, color));
                // Close the color submenu and the outer context menu.
                ui.close_menu();
                close_menu = true;
              }
            }
          });
          change_color_menu.response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Change group color")
          });

          ui.separator();

          let ungroup = ui.button("Ungroup");
          ungroup.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Ungroup"));
          if ungroup.clicked() {
            ops.push(TabStripOp::Ungroup(group_id));
            close_menu = true;
          }

          let label: &'static str = if collapsed {
            "Expand group"
          } else {
            "Collapse group"
          };
          let collapse_toggle = ui.button(label);
          collapse_toggle.widget_info({
            move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
          });
          if collapse_toggle.clicked() {
            ops.push(TabStripOp::ToggleGroupCollapsed(group_id));
            close_menu = true;
          }

          if close_menu {
            // Close any nested menu state (`menu_button`) too.
            ui.close_menu();
          }
        })
        .inner
    });

    menu_rect = Some(inner.response.rect);

    // Best-effort: close when clicking outside the chip and the popup.
    let clicked_outside = ui.ctx().input(|i| {
      i.pointer.any_pressed()
        && i
          .pointer
          .interact_pos()
          .or_else(|| i.pointer.latest_pos())
          .is_some_and(|pos| {
            !chip_rect.contains(pos) && menu_rect.is_some_and(|rect| !rect.contains(pos))
          })
    });
    if clicked_outside {
      close_menu = true;
    }

    if close_menu {
      menu_state.open = false;
    }
  }

  if menu_open_prev != menu_state.open {
    // Ensure we repaint so the popup opens/closes immediately even in low-event situations (e.g.
    // keyboard-driven open or click-away dismissal).
    ui.ctx().request_repaint();
  }

  ui.ctx().data_mut(|d| {
    d.insert_temp(menu_state_id, menu_state);
  });

  #[cfg(test)]
  ui.ctx().data_mut(|d| {
    d.insert_temp(
      egui::Id::new("test_tab_group_context_menu_open"),
      menu_state.open,
    );
    d.insert_temp(egui::Id::new("test_tab_group_context_menu_rect"), menu_rect);
  });

  super::paint_focus_ring(ui, &response, focus_ring);
}

fn tab_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  tab: &mut BrowserTabState,
  is_active: bool,
  interactive: bool,
  can_close_tabs: bool,
  tab_width: f32,
  favicon_tex: Option<egui::TextureId>,
  chrome: &mut ChromeState,
  focus_ring: FocusRingStyle,
  group_color: Option<Color32>,
  close_t: Option<f32>,
) -> (Rect, Response, Option<ChromeAction>) {
  let closing = close_t.is_some();
  let close_t = close_t.unwrap_or(0.0).clamp(0.0, 1.0);
  let close_opacity = if closing {
    (1.0 - close_t).clamp(0.0, 1.0)
  } else {
    1.0
  };
  let interactive = interactive && !closing;

  let (_, tab_rect) = ui.allocate_space(Vec2::new(tab_width.max(0.0), TAB_HEIGHT));
  let tab_id = tab_strip_tab_widget_id(tab.id);
  let title = tab.display_title();
  let mut response = ui.interact(
    tab_rect,
    tab_id,
    if interactive {
      Sense::click_and_drag()
    } else {
      Sense::hover()
    },
  );
  let hovered = interactive && response.hovered();
  let (has_error, has_warning) = {
    let (err, warn) = tab_status_messages(tab);
    let has_error = err.is_some();
    let has_warning = warn.is_some();
    if hovered {
      if err.is_none() && warn.is_none() {
        response = response.on_hover_text(title);
      } else {
        let mut lines = vec![title.to_string()];
        if let Some(err) = err {
          lines.push(format!("Error: {err}"));
        }
        if let Some(warn) = warn {
          lines.push(format!("Warning: {warn}"));
        }
        response = response.on_hover_text(lines.join("\n"));
      }
    }
    (has_error, has_warning)
  };
  let a11y_label = tab.tab_accessible_label(title, is_active, has_error, has_warning);
  // Note: `egui::WidgetInfo::labeled` takes `impl ToString` and therefore allocates a `String` when
  // the closure is executed. Keep the captured label as `Arc<str>` so registering the closure is
  // allocation-free on the steady-state hot path (the closure only runs when egui requests widget
  // info, e.g. for AccessKit updates).
  response.widget_info({
    let a11y_label = a11y_label;
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label.as_ref())
  });
  let _ = ui.ctx().accesskit_node_builder(response.id, |builder| {
    builder.set_role(accesskit::Role::Tab);
    builder.set_selected(is_active);
    if !interactive {
      builder.set_disabled();
    }
  });
  if interactive {
    super::show_tooltip_on_focus(ui, &response, title);
  }
  // Accessibility: when focus moves to a tab that is currently scrolled out of view, scroll the
  // tab-strip so the focused tab becomes visible.
  if response.has_focus() && !ui.is_rect_visible(tab_rect) {
    response.scroll_to_me(Some(egui::Align::Center));
  }

  let visuals = ui.style().visuals.clone();
  let mut paint_ui = ui.child_ui(tab_rect, egui::Layout::left_to_right(egui::Align::Center));
  paint_ui.set_clip_rect(tab_rect);

  // Micro-interaction: fade hover highlight in/out.
  //
  // Active/non-interactive tabs ignore hover, but we still drive the animation state toward `0.0`
  // so switching states doesn't resurrect a stale hover value.
  let hover_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("hover"),
    hovered && !is_active,
    motion.durations.hover_fade,
  );
  let pressed = interactive && ui.is_enabled() && response.is_pointer_button_down_on();

  // Micro-interaction: fade the active tab background in/out instead of snapping.
  let active_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("active"),
    is_active || pressed,
    motion.durations.tab_underline,
  );

  let inactive_bg = lerp_color(
    visuals.widgets.inactive.bg_fill,
    visuals.widgets.hovered.bg_fill,
    hover_t,
  );
  let mut bg = lerp_color(inactive_bg, visuals.widgets.active.bg_fill, active_t);
  if pressed {
    bg = visuals.widgets.active.bg_fill;
  }
  let rounding = visuals.widgets.inactive.rounding;
  {
    let painter = paint_ui.painter();
    painter.rect_filled(tab_rect, rounding, with_alpha(bg, close_opacity));

    if let Some(color) = group_color {
      painter.rect_stroke(
        tab_rect.shrink(0.5),
        rounding,
        Stroke::new(1.0, with_alpha(color, close_opacity)),
      );
    }

    if pressed {
      let stroke_rect = if group_color.is_some() {
        tab_rect.shrink(1.5)
      } else {
        tab_rect.shrink(0.5)
      };
      let mut stroke = visuals.widgets.active.bg_stroke;
      stroke.color = with_alpha(stroke.color, close_opacity);
      painter.rect_stroke(stroke_rect, rounding, stroke);
    }
  }

  // Favicon.
  let icon_min = Pos2::new(
    tab_rect.min.x + TAB_PADDING_X,
    tab_rect.center().y - ICON_SIZE * 0.5,
  );
  let icon_rect = Rect::from_min_size(icon_min, Vec2::splat(ICON_SIZE));
  if let Some(tex_id) = favicon_tex {
    if paint_ui.is_rect_visible(icon_rect) {
      let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
      paint_ui.painter().image(
        tex_id,
        icon_rect,
        uv,
        with_alpha(Color32::WHITE, close_opacity),
      );
    }
  } else {
    placeholder_favicon(paint_ui.painter(), icon_rect, &visuals, close_opacity);
    // Show a small deterministic glyph so blank favicons are still distinguishable at a glance.
    let mut glyph_buf = [0u8; 4];
    let glyph = title
      .trim()
      .chars()
      .next()
      .unwrap_or('?')
      .to_ascii_uppercase()
      .encode_utf8(&mut glyph_buf);
    paint_ui.painter().text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(12.0),
      with_alpha(visuals.text_color(), 0.75 * close_opacity),
    );
  }

  if tab.loading {
    // Spinner overlay around the favicon.
    let time = if motion.enabled && ui.ctx().style().animation_time > 0.0 {
      ui.ctx().request_repaint();
      paint_ui.input(|i| i.time)
    } else {
      // Reduced-motion: keep the spinner static (and avoid continuous repaints).
      0.0
    };
    paint_spinner(
      paint_ui.painter(),
      icon_rect.expand(2.0),
      time,
      with_alpha(visuals.text_color(), close_opacity),
    );
  }

  let err_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_error"),
    has_error,
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    has_warning,
    motion.durations.progress_fade,
  );
  paint_tab_status_badges(
    paint_ui.painter(),
    icon_rect,
    &visuals,
    err_t * close_opacity,
    warn_t * close_opacity,
  );

  // Close button (only when more than one tab exists).
  let mut close_clicked = false;
  let close_rect = if can_close_tabs && interactive {
    let close_min = Pos2::new(
      tab_rect.max.x - TAB_PADDING_X - CLOSE_BUTTON_SIZE,
      tab_rect.center().y - CLOSE_BUTTON_SIZE * 0.5,
    );
    Some(Rect::from_min_size(
      close_min,
      Vec2::splat(CLOSE_BUTTON_SIZE),
    ))
  } else {
    None
  };

  if let Some(close_rect) = close_rect {
    let close_id = ui.make_persistent_id(("tab_strip_close", tab.id));
    let close_has_focus = ui.ctx().memory(|mem| mem.has_focus(close_id));
    // Consider the tab "close-reveal active" when the tab is active, hovered, or keyboard-focused
    // (either the tab itself or the close button).
    //
    // Keep the interaction rect stable; only the icon painting is animated.
    let close_reveal_target = is_active || hovered || response.has_focus() || close_has_focus;
    let close_resp = ui
      .interact(
        close_rect,
        close_id,
        if close_reveal_target {
          Sense::click()
        } else {
          // When the close icon is hidden (non-active tab, not hovered), avoid stealing pointer
          // clicks from the tab itself, but keep the widget focusable so keyboard traversal remains
          // stable (Tab/Shift+Tab).
          Sense::focusable_noninteractive()
        },
      )
      .on_hover_text("Close tab (Ctrl/Cmd+W)");
    // Accessibility: focus may land directly on the close button during Tab/Shift+Tab traversal.
    // Ensure the focused control is visible within the scroll viewport.
    if close_resp.has_focus() && !ui.is_rect_visible(close_rect) {
      close_resp.scroll_to_me(Some(egui::Align::Center));
    }
    #[cfg(test)]
    {
      store_test_close_id(ui.ctx(), tab.id, close_resp.id);
      store_test_close_rect(ui.ctx(), tab.id, close_rect);
    }
    let close_a11y_label = tab.tab_close_accessible_label(title);
    close_resp.widget_info({
      let close_a11y_label = close_a11y_label;
      move || egui::WidgetInfo::labeled(egui::WidgetType::Button, close_a11y_label.as_ref())
    });
    super::show_tooltip_on_focus(ui, &close_resp, "Close tab (Ctrl/Cmd+W)");
    close_clicked =
      close_reveal_target && (close_resp.clicked() || super::keyboard_activate(ui, &close_resp));

    // Micro-interaction: fade close button hover fill in/out.
    let close_rounding =
      egui::Rounding::same((visuals.widgets.inactive.rounding.nw * 0.8).clamp(4.0, 6.0));
    let close_pressed =
      close_reveal_target && ui.is_enabled() && close_resp.is_pointer_button_down_on();
    let close_hover_t = motion.animate_bool(
      ui.ctx(),
      close_id.with("hover"),
      close_resp.hovered(),
      motion.durations.hover_fade,
    );
    if close_pressed {
      paint_ui.painter().rect(
        close_rect,
        close_rounding,
        visuals.widgets.active.bg_fill,
        visuals.widgets.active.bg_stroke,
      );
    } else if close_hover_t > 0.0 {
      paint_ui.painter().rect_filled(
        close_rect,
        close_rounding,
        with_alpha(
          visuals.widgets.hovered.bg_fill.gamma_multiply(0.85),
          close_hover_t,
        ),
      );
    }

    // Micro-interaction: close affordance reveal.
    //
    // Keep reserving the close button space (no layout shift), but animate the icon painting inside
    // the reserved hit-target rect so it feels more "premium" than a simple opacity toggle.
    let close_reveal_t = motion.animate_bool(
      ui.ctx(),
      close_id.with("reveal"),
      close_reveal_target,
      motion.durations.hover_fade,
    );
    if close_reveal_t > 0.0 {
      // Small easing curve so slide/scale doesn't feel linear.
      let t = {
        let t = close_reveal_t.clamp(0.0, 1.0);
        // Smoothstep (ease-in-out): 3t² - 2t³.
        t * t * (3.0 - 2.0 * t)
      };
      let offset_x = (1.0 - t) * 4.0;
      let scale = lerp(0.9, 1.0, t);
      let icon_rect = close_rect.translate(Vec2::new(offset_x, 0.0));
      paint_icon_in_rect(
        &paint_ui,
        icon_rect,
        BrowserIcon::CloseTab,
        ICON_SIZE * scale,
        with_alpha(visuals.text_color(), t),
      );
    }

    super::paint_focus_ring(ui, &close_resp, focus_ring);
  }

  // Title.
  let title_start_x = icon_rect.max.x + ICON_GAP;
  let title_end_x = close_rect
    .map(|r| r.min.x - ICON_GAP)
    .unwrap_or(tab_rect.max.x - TAB_PADDING_X);
  if title_end_x > title_start_x + 4.0 {
    let title_rect = Rect::from_min_max(
      Pos2::new(title_start_x, tab_rect.min.y),
      Pos2::new(title_end_x, tab_rect.max.y),
    );
    if paint_ui.is_rect_visible(title_rect) {
      let label = {
        let mut text = egui::RichText::new(title);
        if is_active {
          text = text.strong();
        }
        if closing {
          text = text.color(with_alpha(visuals.text_color(), close_opacity));
        }
        egui::Label::new(text).truncate(true).wrap(false)
      };
      let _ = paint_ui.put(title_rect, label);
    }
  }

  if closing {
    return (tab_rect, response, None);
  }

  // Input semantics.
  // Open the tab context menu via keyboard: Shift+F10 while the tab has focus (Windows-style
  // context menu gesture).
  //
  // Egui does not currently provide a built-in keyboard activation path for `Response::context_menu`,
  // so we handle it explicitly and forward the signal to `chrome_ui` (which renders the actual
  // popup).
  let open_by_keyboard = interactive
    && response.has_focus()
    && ui.input_mut(|i| {
      i.consume_key(
        egui::Modifiers {
          shift: true,
          ..Default::default()
        },
        egui::Key::F10,
      )
    });
  if open_by_keyboard {
    // Anchor the menu below the tab when opened via keyboard (no cursor position).
    chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
      tab_id: tab.id,
      anchor_points: (tab_rect.left(), tab_rect.bottom()),
      opener_focus: Some(UiFocusToken(tab.id.0)),
    });
    chrome.tab_context_menu_rect = None;

    // Tell the popup renderer to focus the first item so keyboard navigation starts inside the menu.
    let menu_id = egui::Id::new(("tab_context_menu", tab.id));
    ui.ctx()
      .data_mut(|d| d.insert_temp(menu_id.with("opened_via_keyboard"), true));
    return (tab_rect, response, None);
  }
  if response.clicked_by(egui::PointerButton::Secondary) {
    if let Some(pos) = response
      .interact_pointer_pos()
      .or_else(|| ui.input(|i| i.pointer.hover_pos()))
    {
      chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
        tab_id: tab.id,
        anchor_points: (pos.x, pos.y),
        opener_focus: Some(UiFocusToken(tab.id.0)),
      });
      chrome.tab_context_menu_rect = None;
    }
    return (tab_rect, response, None);
  }
  if close_clicked && can_close_tabs {
    chrome.open_tab_context_menu = None;
    chrome.tab_context_menu_rect = None;
    return (tab_rect, response, Some(ChromeAction::CloseTab(tab.id)));
  }
  if response.clicked_by(egui::PointerButton::Middle) {
    if can_close_tabs {
      chrome.open_tab_context_menu = None;
      chrome.tab_context_menu_rect = None;
      return (tab_rect, response, Some(ChromeAction::CloseTab(tab.id)));
    }
  } else if response.clicked() || super::keyboard_activate(ui, &response) {
    chrome.open_tab_context_menu = None;
    chrome.tab_context_menu_rect = None;
    return (tab_rect, response, Some(ChromeAction::ActivateTab(tab.id)));
  }

  super::paint_focus_ring(ui, &response, focus_ring);

  (tab_rect, response, None)
}

fn pinned_tab_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  tab: &mut BrowserTabState,
  is_active: bool,
  can_close_tabs: bool,
  tab_width: f32,
  favicon_tex: Option<egui::TextureId>,
  chrome: &mut ChromeState,
  focus_ring: FocusRingStyle,
  close_t: Option<f32>,
) -> (Rect, Response, Option<ChromeAction>) {
  let closing = close_t.is_some();
  let close_t = close_t.unwrap_or(0.0).clamp(0.0, 1.0);
  let close_opacity = if closing {
    (1.0 - close_t).clamp(0.0, 1.0)
  } else {
    1.0
  };

  let (_, tab_rect) = ui.allocate_space(Vec2::new(tab_width.max(0.0), TAB_HEIGHT));
  let tab_id = tab_strip_tab_widget_id(tab.id);
  let title = tab.display_title();
  let mut response = ui.interact(
    tab_rect,
    tab_id,
    if closing {
      Sense::hover()
    } else {
      Sense::click_and_drag()
    },
  );
  let (has_error, has_warning) = {
    let (err, warn) = tab_status_messages(tab);
    let has_error = err.is_some();
    let has_warning = warn.is_some();
    if !closing && response.hovered() {
      if err.is_none() && warn.is_none() {
        response = response.on_hover_text(title);
      } else {
        let mut lines = vec![title.to_string()];
        if let Some(err) = err {
          lines.push(format!("Error: {err}"));
        }
        if let Some(warn) = warn {
          lines.push(format!("Warning: {warn}"));
        }
        response = response.on_hover_text(lines.join("\n"));
      }
    }
    (has_error, has_warning)
  };
  let a11y_label = tab.tab_accessible_label(title, is_active, has_error, has_warning);
  // See note in `tab_ui`: constructing `WidgetInfo` allocates when the closure is executed.
  response.widget_info({
    let a11y_label = a11y_label;
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label.as_ref())
  });
  let _ = ui.ctx().accesskit_node_builder(response.id, |builder| {
    builder.set_role(accesskit::Role::Tab);
    builder.set_selected(is_active);
    if closing {
      builder.set_disabled();
    }
  });
  if !closing {
    super::show_tooltip_on_focus(ui, &response, title);
  }
  // Accessibility: keep the focused pinned tab visible when the pinned segment overflows and the
  // focused tab is currently out of view.
  if response.has_focus() && !ui.is_rect_visible(tab_rect) {
    response.scroll_to_me(Some(egui::Align::Center));
  }

  let visuals = ui.style().visuals.clone();

  // Micro-interaction: fade hover highlight in/out (active tabs ignore hover).
  let hover_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("hover"),
    !closing && response.hovered() && !is_active,
    motion.durations.hover_fade,
  );
  let pressed = !closing && ui.is_enabled() && response.is_pointer_button_down_on();

  // Micro-interaction: fade the active tab background in/out instead of snapping.
  let active_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("active"),
    is_active || pressed,
    motion.durations.tab_underline,
  );

  let inactive_bg = lerp_color(
    visuals.widgets.inactive.bg_fill,
    visuals.widgets.hovered.bg_fill,
    hover_t,
  );
  let mut bg = lerp_color(inactive_bg, visuals.widgets.active.bg_fill, active_t);
  if pressed {
    bg = visuals.widgets.active.bg_fill;
  }
  let rounding = visuals.widgets.inactive.rounding;
  ui.painter()
    .rect_filled(tab_rect, rounding, with_alpha(bg, close_opacity));
  if pressed {
    let mut stroke = visuals.widgets.active.bg_stroke;
    stroke.color = with_alpha(stroke.color, close_opacity);
    ui.painter()
      .rect_stroke(tab_rect.shrink(0.5), rounding, stroke);
  }

  // Favicon (centered).
  let icon_min = Pos2::new(
    tab_rect.center().x - ICON_SIZE * 0.5,
    tab_rect.center().y - ICON_SIZE * 0.5,
  );
  let icon_rect = Rect::from_min_size(icon_min, Vec2::splat(ICON_SIZE));
  if let Some(tex_id) = favicon_tex {
    if ui.is_rect_visible(icon_rect) {
      let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
      ui.painter().image(
        tex_id,
        icon_rect,
        uv,
        with_alpha(Color32::WHITE, close_opacity),
      );
    }
  } else {
    placeholder_favicon(ui.painter(), icon_rect, &visuals, close_opacity);
    let mut glyph_buf = [0u8; 4];
    let glyph = title
      .trim()
      .chars()
      .next()
      .unwrap_or('?')
      .to_ascii_uppercase()
      .encode_utf8(&mut glyph_buf);
    ui.painter().text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(14.0),
      with_alpha(visuals.text_color(), close_opacity),
    );
  }

  if tab.loading {
    // Spinner overlay around the favicon.
    let time = if motion.enabled && ui.ctx().style().animation_time > 0.0 {
      ui.ctx().request_repaint();
      ui.input(|i| i.time)
    } else {
      // Reduced-motion: keep the spinner static (and avoid continuous repaints).
      0.0
    };
    paint_spinner(
      ui.painter(),
      icon_rect.expand(2.0),
      time,
      with_alpha(visuals.text_color(), close_opacity),
    );
  }

  let err_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_error"),
    has_error,
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    has_warning,
    motion.durations.progress_fade,
  );
  paint_tab_status_badges(
    ui.painter(),
    icon_rect,
    &visuals,
    err_t * close_opacity,
    warn_t * close_opacity,
  );

  if closing {
    return (tab_rect, response, None);
  }

  // Input semantics.
  // Keyboard context menu gesture (Shift+F10) when the tab has focus.
  let open_by_keyboard = response.has_focus()
    && ui.input_mut(|i| {
      i.consume_key(
        egui::Modifiers {
          shift: true,
          ..Default::default()
        },
        egui::Key::F10,
      )
    });
  if open_by_keyboard {
    chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
      tab_id: tab.id,
      anchor_points: (tab_rect.left(), tab_rect.bottom()),
      opener_focus: Some(UiFocusToken(tab.id.0)),
    });
    chrome.tab_context_menu_rect = None;

    let menu_id = egui::Id::new(("tab_context_menu", tab.id));
    ui.ctx()
      .data_mut(|d| d.insert_temp(menu_id.with("opened_via_keyboard"), true));
    return (tab_rect, response, None);
  }
  if response.clicked_by(egui::PointerButton::Secondary) {
    if let Some(pos) = response
      .interact_pointer_pos()
      .or_else(|| ui.input(|i| i.pointer.hover_pos()))
    {
      chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
        tab_id: tab.id,
        anchor_points: (pos.x, pos.y),
        opener_focus: Some(UiFocusToken(tab.id.0)),
      });
      chrome.tab_context_menu_rect = None;
    }
    return (tab_rect, response, None);
  }
  if response.clicked_by(egui::PointerButton::Middle) {
    if can_close_tabs {
      chrome.open_tab_context_menu = None;
      chrome.tab_context_menu_rect = None;
      return (tab_rect, response, Some(ChromeAction::CloseTab(tab.id)));
    }
  } else if response.clicked() || super::keyboard_activate(ui, &response) {
    chrome.open_tab_context_menu = None;
    chrome.tab_context_menu_rect = None;
    return (tab_rect, response, Some(ChromeAction::ActivateTab(tab.id)));
  }

  super::paint_focus_ring(ui, &response, focus_ring);
  (tab_rect, response, None)
}

pub(super) fn tab_strip_ui(
  ui: &mut egui::Ui,
  app: &mut BrowserAppState,
  favicon_for_tab: &mut impl FnMut(TabId) -> Option<egui::TextureId>,
  motion: UiMotion,
  focus_ring: FocusRingStyle,
) -> Vec<ChromeAction> {
  let mut actions = Vec::new();

  // Clone the egui context handle so we don't hold an `&Context` borrow of `ui` across later
  // `&mut Ui` calls (e.g. `child_ui`), which would otherwise trip the borrow checker.
  let ctx = ui.ctx().clone();
  let motion_enabled = motion.enabled && ctx.style().animation_time > 0.0;
  let now = ui.input(|i| i.time);
  let perf_enabled = tab_strip_perf_enabled();
  let perf_start = perf_enabled.then_some(std::time::Instant::now());

  #[cfg(test)]
  ui.ctx().data_mut(|d| {
    d.insert_temp(
      egui::Id::new("test_tab_strip_close_ids"),
      Vec::<(TabId, egui::Id)>::new(),
    );
  });

  // Defensive: if the dragged tab was closed mid-drag, clear the drag state.
  if let Some(dragging_tab_id) = app.chrome.dragging_tab_id {
    if app.tab(dragging_tab_id).is_none() {
      app.chrome.clear_tab_drag();
    }
  }

  // Track close animations stored in chrome UI state so tab closes can be animated consistently
  // regardless of trigger (tab strip, keyboard shortcut, menu bar, etc).
  let mut close_progress: HashMap<TabId, f32> =
    HashMap::with_capacity(app.chrome.closing_tabs.len());
  let mut any_closing_tab_animating = false;
  {
    let (tabs, chrome) = (&app.tabs, &mut app.chrome);
    chrome.closing_tabs.retain(|tab_id, state| {
      let tab_id = *tab_id;
      let exists = tabs.iter().any(|t| t.id == tab_id);
      if !exists {
        return false;
      }

      if !motion_enabled {
        // Animations disabled: treat any in-progress close animation as finished.
        close_progress.insert(tab_id, 1.0);
        actions.push(ChromeAction::CloseTab(tab_id));
        return false;
      }

      let t = state.progress(now).clamp(0.0, 1.0);
      close_progress.insert(tab_id, t);

      if t < 1.0 - 1e-4 {
        any_closing_tab_animating = true;
      } else {
        // Animation finished: request the actual close via the shared action path.
        actions.push(ChromeAction::CloseTab(tab_id));
      }

      // Keep the state until the tab is actually removed; otherwise the tab could briefly
      // re-appear if the close action is delayed.
      true
    });
  }

  let strip_width = ui.available_width().max(0.0);
  let (_, strip_rect) = ui.allocate_space(Vec2::new(strip_width, TAB_STRIP_HEIGHT));
  let button_size = CONTROL_BUTTON_SIZE.min(strip_rect.width().max(0.0));
  let button_rect = Rect::from_center_size(
    Pos2::new(strip_rect.max.x - button_size * 0.5, strip_rect.center().y),
    Vec2::splat(button_size),
  );
  let tabs_viewport_max_x = (button_rect.min.x - 8.0).max(strip_rect.min.x);
  let tabs_rect = Rect::from_min_max(
    strip_rect.min,
    Pos2::new(tabs_viewport_max_x, strip_rect.max.y),
  );
  let tabs_viewport_width = tabs_rect.width().max(0.0);

  let tab_count = app.tabs.len();
  let can_close_tabs = tab_count > 1;
  let pinned_len = app.tabs.iter().take_while(|t| t.pinned).count();
  let pinned_count = pinned_len;
  let unpinned_count = tab_count.saturating_sub(pinned_len);
  let active_id = app.active_tab_id();
  let last_active_id_key = ui.make_persistent_id("tab_strip_last_active");
  let last_active_id = ui
    .ctx()
    .data(|d| d.get_temp::<Option<TabId>>(last_active_id_key))
    .unwrap_or(None);
  let active_changed = active_id != last_active_id;
  ui.ctx()
    .data_mut(|d| d.insert_temp(last_active_id_key, active_id));
  let snapshot_key = ui.make_persistent_id("tab_strip_layout_snapshot");
  let anim_key = ui.make_persistent_id("tab_strip_pin_anim");
  let drag_lift_out_key = ui.make_persistent_id("tab_strip_drag_lift_out");
  let group_chip_width_cache_key = ui.make_persistent_id("tab_strip_group_chip_width_cache");

  // `TabStripLayoutSnapshot` contains per-tab vectors + hashmaps. Cloning it each frame (via
  // `egui::Data::get_temp`) becomes a dominant allocation + CPU cost once tab counts reach
  // 100-500.
  //
  // Instead, we *take* ownership of the previous snapshot from egui memory, reuse its
  // allocations, and store it back at the end of the frame.
  let mut prev_snapshot: Option<TabStripLayoutSnapshot> = ctx.data_mut(|d| {
    std::mem::take(d.get_temp_mut_or_default::<Option<TabStripLayoutSnapshot>>(snapshot_key))
  });
  let mut pin_anim: Option<TabPinAnim> =
    ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<Option<TabPinAnim>>(anim_key)));
  let mut group_chip_width_cache: GroupChipWidthCache = ctx.data_mut(|d| {
    std::mem::take(d.get_temp_mut_or_default::<GroupChipWidthCache>(group_chip_width_cache_key))
  });

  // If animations are disabled or the tab disappeared, snap to the new state.
  if !motion_enabled {
    pin_anim = None;
  } else if let Some(anim) = &pin_anim {
    if app.tab(anim.tab_id).is_none() {
      pin_anim = None;
    }
  }

  // Detect pin/unpin transitions by diffing against the previous frame's layout snapshot.
  //
  // NOTE: this used to scan the full tab list and do an O(pinned_count) search for each tab id to
  // determine whether it was pinned in the previous frame. With hundreds of tabs this becomes a
  // noticeable per-frame CPU cost even when no pin/unpin is happening.
  //
  // Because pinned tabs are always a contiguous prefix of `BrowserAppState::tabs`, we can detect a
  // single-tab pin/unpin transition by comparing `pinned_count` and the pinned-id prefixes between
  // frames.
  if pin_anim.is_none() && motion_enabled {
    if let Some(prev) = prev_snapshot.as_ref() {
      let prev_tab_count = prev.pinned_count + prev.unpinned_count;
      if prev_tab_count == tab_count {
        let prev_pinned_count = prev.pinned_count;
        let pinned_delta = pinned_count as isize - prev_pinned_count as isize;
        let (tab_id, kind, source_index): (TabId, TabPinAnimKind, Option<usize>) =
          match pinned_delta {
            1 => {
              // One tab was pinned: find the id present in the current pinned prefix that wasn't in
              // the previous pinned set.
              let mut added: Option<TabId> = None;
              let mut added_count = 0usize;
              for tab in app.tabs.iter().take(pinned_count) {
                if !prev.pinned_tabs.iter().any(|id| *id == tab.id) {
                  added = Some(tab.id);
                  added_count += 1;
                  if added_count > 1 {
                    break;
                  }
                }
              }
              if added_count != 1 {
                // Either no new pinned id (unexpected) or multiple changes: don't animate.
                (TabId(0), TabPinAnimKind::Pin, None)
              } else {
                let tab_id = added.unwrap_or(TabId(0));
                let source_index = prev.unpinned_items.iter().position(|item| match item {
                  TabStripItemKey::Tab(id) => *id == tab_id,
                  _ => false,
                });
                (tab_id, TabPinAnimKind::Pin, source_index)
              }
            }
            -1 => {
              // One tab was unpinned: find the id present in the previous pinned set that is no
              // longer in the current pinned prefix.
              let mut removed: Option<TabId> = None;
              let mut removed_count = 0usize;
              for id in &prev.pinned_tabs {
                if !app.tabs.iter().take(pinned_count).any(|t| t.id == *id) {
                  removed = Some(*id);
                  removed_count += 1;
                  if removed_count > 1 {
                    break;
                  }
                }
              }
              if removed_count != 1 {
                (TabId(0), TabPinAnimKind::Unpin, None)
              } else {
                let tab_id = removed.unwrap_or(TabId(0));
                let source_index = prev.pinned_tabs.iter().position(|id| *id == tab_id);
                (tab_id, TabPinAnimKind::Unpin, source_index)
              }
            }
            _ => (TabId(0), TabPinAnimKind::Pin, None),
          };

        if tab_id != TabId(0) {
          let from_rect = prev
            .tab_rects
            .iter()
            .find(|(id, _)| *id == tab_id)
            .map(|(_, rect)| *rect);
          if let (Some(from_rect), Some(source_index)) = (from_rect, source_index) {
            pin_anim = Some(TabPinAnim {
              tab_id,
              kind,
              start_time: now,
              duration: motion.durations.tab_pin,
              from_rect,
              source_index,
              from_pinned_count: prev.pinned_count,
              from_unpinned_count: prev.unpinned_count,
              from_unpinned_tab_width: prev.unpinned_tab_width,
            });
          }
        }
      }
    }
  }

  let mut pin_t_raw: f32 = 1.0;
  let mut pin_t: f32 = 1.0;
  if let Some(anim) = &pin_anim {
    let dur = anim.duration;
    if motion_enabled && dur > 0.0 {
      let elapsed = (now - anim.start_time).max(0.0) as f32;
      pin_t_raw = (elapsed / dur).clamp(0.0, 1.0);
      pin_t = ease_in_out_cubic(pin_t_raw);
      if pin_t_raw < 1.0 {
        ctx.request_repaint();
      }
    }
  }
  let pin_anim_render = pin_anim.as_ref().filter(|_| pin_t_raw < 1.0);

  let pinned_content_width = if pinned_count == 0 {
    0.0
  } else {
    let mut width = 0.0_f32;
    for idx in 0..pinned_count {
      let tab_id = app.tabs[idx].id;
      let close_t = close_progress.get(&tab_id).copied().unwrap_or(0.0);
      let frac = (1.0 - close_t).clamp(0.0, 1.0);
      width += PINNED_TAB_WIDTH * frac;
      if idx + 1 < pinned_count {
        width += TAB_GAP;
      }
    }
    width
  };
  let (pinned_viewport_width_final, reserved_unpinned_viewport_width_final) =
    compute_pinned_viewport_width(
      tabs_viewport_width,
      pinned_content_width,
      unpinned_count > 0,
    );
  // `compute_pinned_viewport_width` may drop the inter-segment gap under very narrow widths. Infer
  // the effective gap from the remaining space.
  let segment_gap_final = if pinned_count > 0 && unpinned_count > 0 {
    (tabs_viewport_width - pinned_viewport_width_final - reserved_unpinned_viewport_width_final)
      .max(0.0)
  } else {
    0.0
  };

  let final_pinned_viewport_max_x =
    (tabs_rect.min.x + pinned_viewport_width_final).min(tabs_rect.max.x);
  let final_unpinned_viewport_min_x =
    (final_pinned_viewport_max_x + segment_gap_final).min(tabs_rect.max.x);
  let final_unpinned_viewport_width = (tabs_rect.max.x - final_unpinned_viewport_min_x).max(0.0);

  let mut pinned_viewport_width = pinned_viewport_width_final;
  let mut segment_gap = segment_gap_final;
  if let Some(anim) = &pin_anim {
    let from_pinned_content_width = if anim.from_pinned_count == 0 {
      0.0
    } else {
      (anim.from_pinned_count as f32) * PINNED_TAB_WIDTH
        + (anim.from_pinned_count.saturating_sub(1) as f32) * TAB_GAP
    };
    let (from_pinned_viewport_width, from_reserved_unpinned_viewport_width) =
      compute_pinned_viewport_width(
        tabs_viewport_width,
        from_pinned_content_width,
        anim.from_unpinned_count > 0,
      );
    let from_segment_gap = if anim.from_pinned_count > 0 && anim.from_unpinned_count > 0 {
      (tabs_viewport_width - from_pinned_viewport_width - from_reserved_unpinned_viewport_width)
        .max(0.0)
    } else {
      0.0
    };
    pinned_viewport_width = lerp(
      from_pinned_viewport_width,
      pinned_viewport_width_final,
      pin_t,
    );
    segment_gap = lerp(from_segment_gap, segment_gap_final, pin_t);
  }

  let pinned_viewport_max_x = (tabs_rect.min.x + pinned_viewport_width).min(tabs_rect.max.x);
  let pinned_viewport_rect = Rect::from_min_max(
    tabs_rect.min,
    Pos2::new(pinned_viewport_max_x, tabs_rect.max.y),
  );

  let unpinned_viewport_min_x = (pinned_viewport_max_x + segment_gap).min(tabs_rect.max.x);
  let unpinned_viewport_rect = Rect::from_min_max(
    Pos2::new(unpinned_viewport_min_x, tabs_rect.min.y),
    tabs_rect.max,
  );
  let unpinned_viewport_width = unpinned_viewport_rect.width().max(0.0);

  // Precompute tab strip sizing for the unpinned segment.
  //
  // While a tab group is collapsing/expanding we treat its member tabs as "fractional tabs" so the
  // base `tab_width` (and therefore the widths of *other* tabs) can adjust smoothly instead of
  // jumping at the end of the animation.
  let mut tab_units: f32 = 0.0;
  let mut group_chip_total_width: f32 = 0.0;
  let group_chip_font_id = egui::TextStyle::Button.resolve(ui.style());
  let mut total_gap_width: f32 = 0.0;
  let mut first_item = true;
  let mut group_animating = false;
  // Gap scaling is driven by the *previous* item: gaps after group-member tabs shrink with the
  // group, while gaps after chips/normal tabs remain full. This avoids a chip→next-item "pop" at
  // the end of collapse while still shrinking intra-group gaps.
  let mut prev_gap_scale: Option<f32> = None;
  {
    let mut idx = pinned_len;
    let mut current_group_id: Option<TabGroupId> = None;
    let mut current_group_t: f32 = 1.0;
    let mut current_group_collapsed = false;
    while idx < app.tabs.len() {
      let tab = &app.tabs[idx];
      let Some(group_id) = tab.group else {
        let close_scale = close_progress
          .get(&tab.id)
          .copied()
          .map(|t| (1.0 - t).clamp(0.0, 1.0))
          .unwrap_or(1.0);
        if !first_item {
          total_gap_width += TAB_GAP * prev_gap_scale.unwrap_or(1.0).clamp(0.0, 1.0);
        }
        first_item = false;
        tab_units += close_scale;
        // Normal tabs leave a full-sized gap after them, but closing tabs should shrink their
        // trailing gap so we don't leave behind fixed `TAB_GAP` holes during the collapse.
        prev_gap_scale = close_progress.contains_key(&tab.id).then_some(close_scale);
        idx += 1;
        continue;
      };

      // If the group metadata is missing, treat the tab as ungrouped so we stay robust.
      let Some(group) = app.tab_groups.get(&group_id) else {
        let close_scale = close_progress
          .get(&tab.id)
          .copied()
          .map(|t| (1.0 - t).clamp(0.0, 1.0))
          .unwrap_or(1.0);
        if !first_item {
          total_gap_width += TAB_GAP * prev_gap_scale.unwrap_or(1.0).clamp(0.0, 1.0);
        }
        first_item = false;
        tab_units += close_scale;
        prev_gap_scale = close_progress.contains_key(&tab.id).then_some(close_scale);
        idx += 1;
        continue;
      };

      let is_first = idx == pinned_len || app.tabs[idx - 1].group != Some(group_id);
      if is_first {
        let title = group_chip_title(group);
        let w = group_chip_width_cache.width(ui, group_id, title, &group_chip_font_id);
        group_chip_total_width += w;
        if !first_item {
          total_gap_width += TAB_GAP * prev_gap_scale.unwrap_or(1.0).clamp(0.0, 1.0);
        }
        first_item = false;
        // Chips never scale the gap after them.
        prev_gap_scale = None;

        current_group_id = Some(group_id);
        current_group_collapsed = group.collapsed;
        let expand_id = ui.make_persistent_id(("tab_group_expand", group_id.0));
        let t = motion.animate_bool(
          ui.ctx(),
          expand_id,
          !group.collapsed,
          motion.durations.tab_group_collapse,
        );
        current_group_t = t.clamp(0.0, 1.0);
        let target = if group.collapsed { 0.0 } else { 1.0 };
        if motion.enabled
          && ui.ctx().style().animation_time > 0.0
          && (current_group_t - target).abs() > GROUP_COLLAPSE_HIDE_EPS
        {
          group_animating = true;
        }
      } else if current_group_id != Some(group_id) {
        // Groups are expected to be contiguous. If we encounter a non-contiguous group, re-seed the
        // cached animation value so sizing remains robust.
        current_group_id = Some(group_id);
        current_group_collapsed = group.collapsed;
        let expand_id = ui.make_persistent_id(("tab_group_expand", group_id.0));
        let t = motion.animate_bool(
          ui.ctx(),
          expand_id,
          !group.collapsed,
          motion.durations.tab_group_collapse,
        );
        current_group_t = t.clamp(0.0, 1.0);
      }

      let group_t = current_group_t;
      let collapsed = current_group_collapsed;

      if collapsed && group_t <= GROUP_COLLAPSE_HIDE_EPS {
        while idx < app.tabs.len() && app.tabs[idx].group == Some(group_id) {
          idx += 1;
        }
        continue;
      }

      let close_scale = close_progress
        .get(&tab.id)
        .copied()
        .map(|t| (1.0 - t).clamp(0.0, 1.0))
        .unwrap_or(1.0);
      let scale = (group_t * close_scale).clamp(0.0, 1.0);

      if !first_item {
        total_gap_width += TAB_GAP * prev_gap_scale.unwrap_or(1.0).clamp(0.0, 1.0);
      }
      first_item = false;
      tab_units += scale;
      prev_gap_scale = Some(scale);
      idx += 1;
    }
  }
  if group_animating {
    ui.ctx().request_repaint();
  }
  let sizing_target = compute_tab_strip_sizing_with_scaled_tabs(
    final_unpinned_viewport_width,
    tab_units,
    group_chip_total_width,
    total_gap_width,
  );
  // Target unpinned tab width for this frame. When a tab is being pinned/unpinned, we already have
  // a dedicated width animation to avoid "jumping" due to the unpinned count changing.
  let tab_width_target = if let Some(anim) = &pin_anim {
    lerp(anim.from_unpinned_tab_width, sizing_target.tab_width, pin_t)
  } else {
    sizing_target.tab_width
  };
  let tab_width_target = sanitize_tab_width(tab_width_target);

  // Smoothly animate global tab width reflows (e.g. window resize or open/close tabs) so tabs don't
  // snap-resize. Pin/unpin already animates width, so avoid double-easing in that case.
  let tab_width = if tab_units <= 0.0 || pin_anim.is_some() {
    tab_width_target
  } else {
    let id = egui::Id::new("tab_strip_tab_width");
    let width = motion.animate_f32(&ctx, id, tab_width_target, motion.durations.tab_width);
    let width = sanitize_tab_width(width);
    if motion_enabled && motion.durations.tab_width > 0.0 && (width - tab_width_target).abs() > 0.01
    {
      ctx.request_repaint();
    }
    width
  };

  let total_content_width = tab_width * tab_units + group_chip_total_width + total_gap_width;
  let overflow = total_content_width > unpinned_viewport_width + 0.01;
  let sizing = TabStripSizing {
    tab_width,
    overflow,
    total_content_width,
  };

  let mut ops: Vec<TabStripOp> = Vec::new();

  #[cfg(test)]
  let mut tab_rects_for_test: Vec<Rect> = Vec::new();

  // Drag-to-reorder needs the ordered list of visible tab rects. With hundreds of tabs, building
  // and allocating these vectors every frame is expensive, so we only populate them when a drag is
  // actually active.
  let mut pinned_tab_rects_for_drag: Vec<(TabId, Rect)> = Vec::new();
  let mut unpinned_tab_rects_for_drag: Vec<(TabId, Rect)> = Vec::new();
  let mut dragged_tab_rect: Option<Rect> = None;

  let mut active_tab_rect: Option<Rect> = None;
  let mut active_tab_is_pinned = false;
  let mut pinned_scroll_offset_x: f32 = 0.0;
  let mut pinned_max_scroll_x: f32 = 0.0;
  let mut pinned_scroll_viewport_rect: Option<Rect> = None;
  let mut scroll_offset_x: f32 = 0.0;
  let mut unpinned_max_scroll_x: f32 = 0.0;
  let mut unpinned_scroll_viewport_rect: Option<Rect> = None;

  // -----------------------------------------------------------------------------
  // Per-frame scratch storage
  // -----------------------------------------------------------------------------
  //
  // The tab strip is rendered every egui frame. With hundreds of tabs, repeatedly allocating
  // `HashMap`s/`Vec`s (and cloning the previous frame's snapshot out of egui memory) can become a
  // dominant CPU + allocation cost.
  //
  // We store the previous frame's snapshot in egui temp memory and *take* it at the start of the
  // frame. This lets us:
  // - Avoid cloning the full snapshot (deep-clones every vec/map entry).
  // - Reuse the existing `Vec`/`HashMap` allocations by clearing + refilling them.
  let mut layout_snapshot = prev_snapshot.unwrap_or_else(|| TabStripLayoutSnapshot {
    pinned_tabs: Vec::new(),
    unpinned_items: Vec::new(),
    tab_rects: Vec::new(),
    unpinned_tab_width: 0.0,
    scroll_offset_x: 0.0,
    pinned_count: 0,
    unpinned_count: 0,
  });

  let mut layout_tab_rects = std::mem::take(&mut layout_snapshot.tab_rects);
  layout_tab_rects.clear();
  layout_tab_rects.reserve(tab_count);

  let mut unpinned_items_for_snapshot = std::mem::take(&mut layout_snapshot.unpinned_items);
  unpinned_items_for_snapshot.clear();
  unpinned_items_for_snapshot.reserve(unpinned_count + app.tab_groups.len());

  let mut pinned_tabs_for_snapshot = std::mem::take(&mut layout_snapshot.pinned_tabs);
  pinned_tabs_for_snapshot.clear();
  pinned_tabs_for_snapshot.reserve(pinned_count);

  // Index into `layout_tab_rects` where the pinned segment ends (unpinned tabs start).
  let mut pinned_tab_rects_end: usize = 0;

  let mut ghost_dest_anchor_rect: Option<Rect> = None;

  if tabs_viewport_width > 0.0 {
    if pinned_viewport_rect.width() > 0.0
      || pin_anim_render.is_some_and(|anim| anim.kind == TabPinAnimKind::Pin)
      || pin_anim_render.is_some_and(|anim| anim.kind == TabPinAnimKind::Unpin)
    {
      let mut pinned_ui = ui.child_ui(
        pinned_viewport_rect,
        egui::Layout::left_to_right(egui::Align::Center),
      );
      pinned_ui.set_clip_rect(pinned_viewport_rect);
      let mut restore_scroll_delta: Option<Vec2> = None;
      let mut restore_wheel_deltas: Option<Vec<Vec2>> = None;
      // Match the unpinned segment ergonomics: treat vertical wheel scroll as horizontal scroll
      // while the pointer is over the pinned strip.
      let pointer_over_strip = pinned_ui.input(|i| {
        i.pointer
          .hover_pos()
          .is_some_and(|pos| pinned_viewport_rect.contains(pos))
      });
      if pointer_over_strip {
        let has_vertical_scroll = pinned_ui.input(|i| {
          i.scroll_delta.y.abs() > 0.0
            || i.events.iter().any(|event| {
              matches!(
                event,
                egui::Event::MouseWheel { delta, .. } if delta.y.abs() > 0.0
              )
            })
        });
        if has_vertical_scroll {
          pinned_ui.ctx().input_mut(|i| {
            restore_scroll_delta = Some(i.scroll_delta);
            i.scroll_delta = Vec2::new(i.scroll_delta.x + i.scroll_delta.y, 0.0);
            let mut wheel_deltas: Vec<Vec2> = Vec::new();
            for event in &mut i.events {
              if let egui::Event::MouseWheel { delta, .. } = event {
                wheel_deltas.push(*delta);
                let d = *delta;
                *delta = Vec2::new(d.x + d.y, 0.0);
              }
            }
            if !wheel_deltas.is_empty() {
              restore_wheel_deltas = Some(wheel_deltas);
            }
          });
        }
      }

      let scroll_output = egui::ScrollArea::horizontal()
        .id_source("tab_strip_pinned_scroll")
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut pinned_ui, |ui| {
          // We apply gaps manually so we can animate pin/unpin placeholder gaps without leaving
          // behind fixed `TAB_GAP` spacers.
          ui.spacing_mut().item_spacing = Vec2::ZERO;
          ui.horizontal(|ui| {
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum GapKind {
              Normal,
              SourcePlaceholder,
              DestPlaceholder,
            }

            let mut prev_kind: Option<GapKind> = None;
            let gap_before =
              |prev: GapKind, current: GapKind, current_is_last_source: bool| -> f32 {
                let mut gap = TAB_GAP;
                if let Some(anim) = pin_anim_render {
                  match anim.kind {
                    TabPinAnimKind::Pin => {
                      // Pinned dest placeholder: grow the gap before it so the pinned segment doesn't
                      // jump when the item is inserted.
                      if current == GapKind::DestPlaceholder {
                        gap = TAB_GAP * pin_t;
                      }
                    }
                    TabPinAnimKind::Unpin => {
                      // Source placeholder: collapse one adjacent gap so removing the tab doesn't
                      // leave behind an extra `TAB_GAP`.
                      if prev == GapKind::SourcePlaceholder {
                        gap = TAB_GAP * (1.0 - pin_t);
                      } else if current == GapKind::SourcePlaceholder && current_is_last_source {
                        gap = TAB_GAP * (1.0 - pin_t);
                      }
                    }
                  }
                }
                gap
              };

            let chrome = &mut app.chrome;
            let mut inserted_source_placeholder = false;
            let source_placeholder_index = pin_anim_render
              .filter(|anim| anim.kind == TabPinAnimKind::Unpin)
              .map(|anim| anim.source_index);

            for idx in 0..pinned_count {
              if let Some(source_idx) = source_placeholder_index {
                if !inserted_source_placeholder && idx == source_idx {
                  if let Some(prev) = prev_kind {
                    ui.add_space(gap_before(prev, GapKind::SourcePlaceholder, false));
                  }
                  let w = PINNED_TAB_WIDTH * (1.0 - pin_t);
                  let (_, rect) = ui.allocate_space(Vec2::new(w, TAB_HEIGHT));
                  #[cfg(test)]
                  tab_rects_for_test.push(rect);
                  prev_kind = Some(GapKind::SourcePlaceholder);
                  inserted_source_placeholder = true;
                }
              }

              let tab = &mut app.tabs[idx];
              let tab_id = tab.id;

              // Pinning: the tab is already in the pinned segment (at its final position). Replace
              // it with a growing placeholder and draw the real tab as a moving ghost above the
              // strip.
              if pin_anim_render
                .is_some_and(|anim| anim.kind == TabPinAnimKind::Pin && anim.tab_id == tab_id)
              {
                if let Some(prev) = prev_kind {
                  ui.add_space(gap_before(prev, GapKind::DestPlaceholder, false));
                }
                let w = PINNED_TAB_WIDTH * pin_t;
                let (_, rect) = ui.allocate_space(Vec2::new(w, TAB_HEIGHT));
                ghost_dest_anchor_rect = Some(Rect::from_min_size(
                  rect.min,
                  Vec2::new(PINNED_TAB_WIDTH, TAB_HEIGHT),
                ));
                let placeholder_id = ui.make_persistent_id(("tab_strip_pin_placeholder", tab_id));
                let resp = ui.interact(rect, placeholder_id, Sense::hover());
                if !ui.is_rect_visible(rect) {
                  resp.scroll_to_me(Some(egui::Align::Center));
                }
                #[cfg(test)]
                tab_rects_for_test.push(rect);
                prev_kind = Some(GapKind::DestPlaceholder);
                continue;
              }

              if let Some(prev) = prev_kind {
                ui.add_space(gap_before(prev, GapKind::Normal, false));
              }

              let is_active = active_id == Some(tab_id);
              let close_t = close_progress.get(&tab_id).copied();
              let tab_width = close_t
                .map(|t| PINNED_TAB_WIDTH * (1.0 - t).clamp(0.0, 1.0))
                .unwrap_or(PINNED_TAB_WIDTH);
              let favicon_tex = favicon_for_tab(tab_id);
              let is_dragged = chrome.dragging_tab_id == Some(tab_id);
              let (tab_rect, tab_response, maybe_action) = if is_dragged {
                // While dragging, keep layout stable but don't paint the tab in-flow.
                let (_, rect) = ui.allocate_space(Vec2::new(tab_width, TAB_HEIGHT));
                (rect, None, None)
              } else {
                let (rect, resp, action) = pinned_tab_ui(
                  ui,
                  motion,
                  tab,
                  is_active,
                  can_close_tabs,
                  tab_width,
                  favicon_tex,
                  chrome,
                  focus_ring,
                  close_t,
                );
                (rect, Some(resp), action)
              };

              layout_tab_rects.push((tab_id, tab_rect));

              if is_active {
                active_tab_rect = Some(tab_rect);
                active_tab_is_pinned = true;
              }
              if is_active && active_changed {
                if let Some(tab_response) = &tab_response {
                  tab_response.scroll_to_me(Some(egui::Align::Center));
                }
              }
              #[cfg(test)]
              tab_rects_for_test.push(tab_rect);
              // Drag rect lists are built lazily (only when dragging).

              let is_close_action = maybe_action
                .as_ref()
                .is_some_and(|action| matches!(action, ChromeAction::CloseTab(_)));
              if !is_close_action {
                if let Some(tab_response) = &tab_response {
                  if tab_response.drag_started() && chrome.dragging_tab_id.is_none() {
                    chrome.dragging_tab_id = Some(tab_id);
                    let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                    chrome.drag_start_pointer_pos = pointer_pos.map(|p| (p.x, p.y));
                    chrome.drag_start_tab_rect = Some((
                      tab_rect.min.x,
                      tab_rect.min.y,
                      tab_rect.max.x,
                      tab_rect.max.y,
                    ));
                    chrome.tab_drag_session = chrome.tab_drag_session.wrapping_add(1);
                  }
                }
              }
              if chrome.dragging_tab_id == Some(tab_id) {
                dragged_tab_rect = Some(tab_rect);
              }
              if let Some(action) = maybe_action {
                actions.push(action);
              }

              prev_kind = Some(GapKind::Normal);
            }

            // Placeholder at end (unpinning the last pinned tab).
            if let Some(source_idx) = source_placeholder_index {
              if !inserted_source_placeholder && source_idx >= pinned_count {
                if let Some(prev) = prev_kind {
                  ui.add_space(gap_before(prev, GapKind::SourcePlaceholder, true));
                }
                let w = PINNED_TAB_WIDTH * (1.0 - pin_t);
                let (_, rect) = ui.allocate_space(Vec2::new(w, TAB_HEIGHT));
                #[cfg(test)]
                tab_rects_for_test.push(rect);
              }
            }
          });
        });
      let mut scroll_state = scroll_output.state;
      pinned_scroll_offset_x = scroll_state.offset.x;
      pinned_max_scroll_x =
        (scroll_output.content_size.x - scroll_output.inner_rect.width()).max(0.0);
      let pinned_scroll_rect = scroll_output.inner_rect;
      pinned_scroll_viewport_rect = Some(pinned_scroll_rect);
      if restore_scroll_delta.is_some() || restore_wheel_deltas.is_some() {
        let restore_scroll_delta = restore_scroll_delta.take();
        let restore_wheel_deltas = restore_wheel_deltas.take();
        pinned_ui.ctx().input_mut(move |i| {
          if let Some(scroll_delta) = restore_scroll_delta {
            i.scroll_delta = scroll_delta;
          }
          if let Some(wheel_deltas) = restore_wheel_deltas {
            let mut deltas = wheel_deltas.into_iter();
            for event in &mut i.events {
              if let egui::Event::MouseWheel { delta, .. } = event {
                if let Some(saved) = deltas.next() {
                  *delta = saved;
                }
              }
            }
          }
        });
      }

      // Auto-scroll pinned strip while drag-reordering a pinned tab.
      if pinned_max_scroll_x > 0.5 {
        if let (Some(dragging_tab_id), Some(pointer_pos)) = (
          app.chrome.dragging_tab_id,
          ui.input(|i| i.pointer.interact_pos()),
        ) {
          let dragging_is_pinned = app.tab(dragging_tab_id).is_some_and(|tab| tab.pinned);
          if dragging_is_pinned {
            let dt = ui.ctx().input(|i| i.stable_dt).clamp(0.0, 0.1);
            let delta_x = drag_autoscroll_delta_x(pointer_pos, pinned_scroll_rect, dt);
            if delta_x != 0.0 {
              let prev = scroll_state.offset.x;
              let next = (prev + delta_x).clamp(0.0, pinned_max_scroll_x);
              if (next - prev).abs() > 0.01 {
                scroll_state.offset.x = next;
                scroll_state.store(ui.ctx(), scroll_output.id);
                ui.ctx().request_repaint();
              }
            }
          }
        }
      }
      // Record the boundary between pinned and unpinned tab rects for this frame (used to slice
      // `layout_tab_rects` when building the drag-to-reorder rect list).
      pinned_tab_rects_end = layout_tab_rects.len();
    }

    if unpinned_count > 0
      || pin_anim_render.is_some_and(|anim| anim.kind == TabPinAnimKind::Pin)
      || pin_anim_render.is_some_and(|anim| anim.kind == TabPinAnimKind::Unpin)
    {
      let mut unpinned_ui = ui.child_ui(
        unpinned_viewport_rect,
        egui::Layout::left_to_right(egui::Align::Center),
      );
      unpinned_ui.set_clip_rect(unpinned_viewport_rect);

      let scroll_clamp_id = unpinned_ui.make_persistent_id("tab_strip_scroll_clamp");
      let desired_scroll_id = scroll_clamp_id.with("desired_scroll_offset_x");
      let clamp_anim_id = scroll_clamp_id.with("scroll_clamp_anim");
      let scroll_state_id_key = scroll_clamp_id.with("scroll_state_id");

      let mut restore_scroll_delta: Option<Vec2> = None;
      let mut restore_wheel_deltas: Option<Vec<Vec2>> = None;
      // Browser-like ergonomics: treat vertical wheel scrolling as horizontal scrolling when the
      // pointer is over the tab strip (so users don't need a trackpad horizontal gesture).
      let pointer_over_strip = unpinned_ui.input(|i| {
        i.pointer
          .hover_pos()
          .is_some_and(|pos| unpinned_viewport_rect.contains(pos))
      });
      if pointer_over_strip {
        let has_vertical_scroll = unpinned_ui.input(|i| {
          i.scroll_delta.y.abs() > 0.0
            || i.events.iter().any(|event| {
              matches!(
                event,
                egui::Event::MouseWheel { delta, .. } if delta.y.abs() > 0.0
              )
            })
        });
        if has_vertical_scroll {
          unpinned_ui.ctx().input_mut(|i| {
            restore_scroll_delta = Some(i.scroll_delta);
            i.scroll_delta = Vec2::new(i.scroll_delta.x + i.scroll_delta.y, 0.0);
            let mut wheel_deltas: Vec<Vec2> = Vec::new();
            for event in &mut i.events {
              if let egui::Event::MouseWheel { delta, .. } = event {
                wheel_deltas.push(*delta);
                let d = *delta;
                *delta = Vec2::new(d.x + d.y, 0.0);
              }
            }
            if !wheel_deltas.is_empty() {
              restore_wheel_deltas = Some(wheel_deltas);
            }
          });
        }
      }

      // Base max scroll (without any temporary clamp spacer). `sizing` already accounts for group
      // chip widths (Task 17), so this stays accurate when groups are present.
      unpinned_max_scroll_x = (sizing.total_content_width - unpinned_viewport_width).max(0.0);

      // Smoothly clamp the scroll offset when content shrinks (e.g. closing tabs at the right
      // end). If we're beyond the new max, animating prevents a noticeable snap.
      let mut end_spacer_x = 0.0_f32;
      let now = unpinned_ui.input(|i| i.time);
      let stored_scroll_state_id = unpinned_ui
        .ctx()
        .data(|d| d.get_temp::<egui::Id>(scroll_state_id_key));
      let mut current_offset_x = stored_scroll_state_id
        .and_then(|id| egui::scroll_area::State::load(unpinned_ui.ctx(), id).map(|s| s.offset.x))
        .unwrap_or(0.0);
      if !current_offset_x.is_finite() {
        current_offset_x = 0.0;
      }

      let mut desired_scroll_offset_x = current_offset_x;
      let mut clamping_scroll = false;

      let (clamped_scroll_offset_x, clamp_end_spacer_x, clamping) = tab_strip_scroll_clamp(
        unpinned_ui.ctx(),
        motion,
        clamp_anim_id,
        desired_scroll_offset_x,
        unpinned_max_scroll_x,
        now,
      );
      desired_scroll_offset_x = clamped_scroll_offset_x;
      end_spacer_x = clamp_end_spacer_x;
      clamping_scroll = clamping;

      if clamping_scroll {
        // Programmatically set the scroll offset for this frame.
        if let Some(scroll_state_id) = stored_scroll_state_id {
          if let Some(mut scroll_state) =
            egui::scroll_area::State::load(unpinned_ui.ctx(), scroll_state_id)
          {
            scroll_state.offset.x = desired_scroll_offset_x;
            scroll_state.store(unpinned_ui.ctx(), scroll_state_id);
          }
        }
      }

      let scroll_output = egui::ScrollArea::horizontal()
        .id_source("tab_strip_scroll")
        .auto_shrink([false, true])
        // The tab strip should look like a real browser: overflow is indicated via the edge fades,
        // not visible scrollbars.
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut unpinned_ui, |ui| {
          // We apply tab gaps explicitly (instead of relying on `item_spacing`) so group collapse can
          // smoothly shrink both tab widths and the intra-group gaps without leaving behind fixed
          // spacers.
          ui.spacing_mut().item_spacing = Vec2::ZERO;
          ui.horizontal(|ui| {
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum GapKind {
              Normal,
              SourcePlaceholder,
              DestPlaceholder,
            }

            let mut idx = pinned_len;
            let mut first_item = true;
            let mut prev_kind: GapKind = GapKind::Normal;

            // Gap scaling is driven by the *previous* item: gaps after group-member tabs shrink
            // with the group, while gaps after chips/normal tabs remain full. This avoids a
            // chip→next-item "pop" at the end of collapse while still shrinking intra-group gaps.
            let mut prev_gap_scale: Option<f32> = None;

            let mut add_gap = |ui: &mut egui::Ui,
                               first_item: &mut bool,
                               prev_kind: GapKind,
                               prev_gap_scale: Option<f32>,
                               curr_kind: GapKind,
                               current_is_last_source: bool| {
              if *first_item {
                *first_item = false;
                return;
              }
              let scale = prev_gap_scale.unwrap_or(1.0).clamp(0.0, 1.0);
              let mut gap = TAB_GAP * scale;
              if let Some(anim) = pin_anim_render {
                match anim.kind {
                  TabPinAnimKind::Pin => {
                    // Removing from unpinned: collapse the gap *after* the placeholder (i.e. the
                    // gap before whatever comes next).
                    if prev_kind == GapKind::SourcePlaceholder {
                      gap *= 1.0 - pin_t;
                    } else if curr_kind == GapKind::SourcePlaceholder && current_is_last_source {
                      // Placeholder inserted at end: collapse the trailing gap.
                      gap *= 1.0 - pin_t;
                    }
                  }
                  TabPinAnimKind::Unpin => {
                    // Inserting into unpinned: grow the gap after the placeholder so existing
                    // content doesn't jump when a new leading item appears.
                    if prev_kind == GapKind::DestPlaceholder {
                      gap *= pin_t;
                    }
                  }
                }
              }
              if gap > 0.0 {
                ui.add_space(gap);
              }
            };

            let mut item_pos: usize = 0;
            let mut inserted_source_placeholder = false;
            let source_placeholder_index = pin_anim_render
              .filter(|anim| anim.kind == TabPinAnimKind::Pin)
              .map(|anim| anim.source_index);
            let source_placeholder_from_w = pin_anim_render
              .filter(|anim| anim.kind == TabPinAnimKind::Pin)
              .map(|anim| anim.from_rect.width())
              .unwrap_or(sizing.tab_width);
            let source_placeholder_scale = pin_anim_render
              .filter(|anim| anim.kind == TabPinAnimKind::Pin)
              .map(|anim| {
                if anim.from_unpinned_tab_width > 0.0 {
                  (anim.from_rect.width() / anim.from_unpinned_tab_width).clamp(0.0, 1.0)
                } else {
                  1.0
                }
              })
              .unwrap_or(1.0);

            macro_rules! maybe_insert_source_placeholder {
              ($is_last:expr) => {
                if let Some(source_idx) = source_placeholder_index {
                  if !inserted_source_placeholder && item_pos == source_idx {
                    add_gap(
                      ui,
                      &mut first_item,
                      prev_kind,
                      prev_gap_scale,
                      GapKind::SourcePlaceholder,
                      $is_last,
                    );
                    let w = source_placeholder_from_w * (1.0 - pin_t);
                    let (_, rect) = ui.allocate_space(Vec2::new(w, TAB_HEIGHT));
                    #[cfg(test)]
                    tab_rects_for_test.push(rect);
                    prev_kind = GapKind::SourcePlaceholder;
                    prev_gap_scale = Some(source_placeholder_scale);
                    inserted_source_placeholder = true;
                    item_pos += 1;
                  }
                }
              };
            }

            let mut current_group_id: Option<TabGroupId> = None;
            let mut current_group_t: f32 = 1.0;
            let mut current_group_collapsed = false;

            while idx < app.tabs.len() {
              maybe_insert_source_placeholder!(false);

              let tab_id = app.tabs[idx].id;
              let tab_group = app.tabs[idx]
                .group
                .filter(|group_id| app.tab_groups.contains_key(group_id));

              if let Some(group_id) = tab_group {
                let is_first = idx == pinned_len || app.tabs[idx - 1].group != Some(group_id);
                if is_first {
                  maybe_insert_source_placeholder!(false);
                  add_gap(
                    ui,
                    &mut first_item,
                    prev_kind,
                    prev_gap_scale,
                    GapKind::Normal,
                    false,
                  );
                  let chip_width = {
                    let group = app
                      .tab_groups
                      .get(&group_id)
                      .expect("tab_group is filtered to existing groups"); // fastrender-allow-unwrap
                    group_chip_width_cache.width(
                      ui,
                      group_id,
                      group_chip_title(group),
                      &group_chip_font_id,
                    )
                  };
                  group_chip_ui(
                    ui,
                    motion,
                    app,
                    group_id,
                    &mut ops,
                    focus_ring,
                    Some(chip_width),
                  );
                  unpinned_items_for_snapshot.push(TabStripItemKey::GroupChip(group_id));
                  prev_kind = GapKind::Normal;
                  prev_gap_scale = None;
                  item_pos += 1;
                }

                if is_first || current_group_id != Some(group_id) {
                  let collapsed = app
                    .tab_groups
                    .get(&group_id)
                    .is_some_and(|group| group.collapsed);
                  let expand_id = ui.make_persistent_id(("tab_group_expand", group_id.0));
                  current_group_t = motion
                    .animate_bool(
                      ui.ctx(),
                      expand_id,
                      !collapsed,
                      motion.durations.tab_group_collapse,
                    )
                    .clamp(0.0, 1.0);
                  current_group_collapsed = collapsed;
                  current_group_id = Some(group_id);
                }
                let collapsed = current_group_collapsed;
                let group_t = current_group_t;
                if collapsed && group_t <= GROUP_COLLAPSE_HIDE_EPS {
                  // Fully collapsed: hide all member tabs (but keep the chip visible).
                  while idx < app.tabs.len() && app.tabs[idx].group == Some(group_id) {
                    idx += 1;
                  }
                  continue;
                }

                let close_t = close_progress.get(&tab_id).copied();
                let close_scale = close_t.map(|t| (1.0 - t).clamp(0.0, 1.0)).unwrap_or(1.0);
                let scale = (group_t * close_scale).clamp(0.0, 1.0);

                maybe_insert_source_placeholder!(false);
                add_gap(
                  ui,
                  &mut first_item,
                  prev_kind,
                  prev_gap_scale,
                  GapKind::Normal,
                  false,
                );
                let interactive = !collapsed && group_t > 0.95 && close_t.is_none();
                let tab_width = sizing.tab_width * scale;
                let is_active = active_id == Some(tab_id);
                let favicon_tex = favicon_for_tab(tab_id);
                let is_dragged = app.chrome.dragging_tab_id == Some(tab_id);
                let (tab_rect, tab_response, maybe_action) = if is_dragged {
                  // While dragging, keep layout stable but don't paint the tab in-flow.
                  let (_, rect) = ui.allocate_space(Vec2::new(tab_width, TAB_HEIGHT));
                  (rect, None, None)
                } else {
                  let tab = &mut app.tabs[idx];
                  let group_border = tab
                    .group
                    .and_then(|gid| app.tab_groups.get(&gid).map(|g| group_color_egui(g.color)));
                  let (rect, resp, action) = tab_ui(
                    ui,
                    motion,
                    tab,
                    is_active,
                    interactive,
                    can_close_tabs,
                    tab_width,
                    favicon_tex,
                    &mut app.chrome,
                    focus_ring,
                    group_border,
                    close_t,
                  );
                  (rect, Some(resp), action)
                };
                layout_tab_rects.push((tab_id, tab_rect));
                if is_active {
                  active_tab_rect = Some(tab_rect);
                  active_tab_is_pinned = false;
                }
                if is_active && active_changed {
                  if let Some(tab_response) = &tab_response {
                    tab_response.scroll_to_me(Some(egui::Align::Center));
                  }
                }
                #[cfg(test)]
                tab_rects_for_test.push(tab_rect);

                unpinned_items_for_snapshot.push(TabStripItemKey::Tab(tab_id));
                item_pos += 1;

                if interactive {
                  // Drag rect lists are built lazily (only when dragging).
                  let is_close_action = maybe_action
                    .as_ref()
                    .is_some_and(|action| matches!(action, ChromeAction::CloseTab(_)));
                  if !is_close_action {
                    if let Some(tab_response) = &tab_response {
                      if tab_response.drag_started() && app.chrome.dragging_tab_id.is_none() {
                        app.chrome.dragging_tab_id = Some(tab_id);
                        let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                        app.chrome.drag_start_pointer_pos = pointer_pos.map(|p| (p.x, p.y));
                        app.chrome.drag_start_tab_rect = Some((
                          tab_rect.min.x,
                          tab_rect.min.y,
                          tab_rect.max.x,
                          tab_rect.max.y,
                        ));
                        app.chrome.tab_drag_session = app.chrome.tab_drag_session.wrapping_add(1);
                      }
                    }
                  }
                }

                if app.chrome.dragging_tab_id == Some(tab_id) {
                  dragged_tab_rect = Some(tab_rect);
                }

                if let Some(action) = maybe_action {
                  actions.push(action);
                }

                prev_kind = GapKind::Normal;
                prev_gap_scale = Some(scale);
                idx += 1;
                continue;
              }

              maybe_insert_source_placeholder!(false);

              // Destination placeholder (unpinning): keep the moving tab non-interactive while it
              // expands into the scrollable segment.
              if pin_anim_render
                .is_some_and(|anim| anim.kind == TabPinAnimKind::Unpin && anim.tab_id == tab_id)
              {
                add_gap(
                  ui,
                  &mut first_item,
                  prev_kind,
                  prev_gap_scale,
                  GapKind::DestPlaceholder,
                  false,
                );
                let w = sizing.tab_width * pin_t;
                let (_, rect) = ui.allocate_space(Vec2::new(w, TAB_HEIGHT));
                ghost_dest_anchor_rect = Some(Rect::from_min_size(
                  rect.min,
                  Vec2::new(sizing_target.tab_width, TAB_HEIGHT),
                ));
                let placeholder_id = ui.make_persistent_id(("tab_strip_pin_placeholder", tab_id));
                let resp = ui.interact(rect, placeholder_id, Sense::hover());
                if !ui.is_rect_visible(rect) {
                  resp.scroll_to_me(Some(egui::Align::Center));
                }
                #[cfg(test)]
                tab_rects_for_test.push(rect);
                unpinned_items_for_snapshot.push(TabStripItemKey::Tab(tab_id));
                prev_kind = GapKind::DestPlaceholder;
                prev_gap_scale = None;
                item_pos += 1;
                idx += 1;
                continue;
              }

              let close_t = close_progress.get(&tab_id).copied();
              let close_scale = close_t.map(|t| (1.0 - t).clamp(0.0, 1.0)).unwrap_or(1.0);
              add_gap(
                ui,
                &mut first_item,
                prev_kind,
                prev_gap_scale,
                GapKind::Normal,
                false,
              );
              let is_active = active_id == Some(tab_id);
              let tab_width = sizing.tab_width * close_scale;
              let favicon_tex = favicon_for_tab(tab_id);
              let is_dragged = app.chrome.dragging_tab_id == Some(tab_id);
              let (tab_rect, tab_response, maybe_action) = if is_dragged {
                // While dragging, keep layout stable but don't paint the tab in-flow.
                let (_, rect) = ui.allocate_space(Vec2::new(tab_width, TAB_HEIGHT));
                (rect, None, None)
              } else {
                let tab = &mut app.tabs[idx];
                let group_border = tab
                  .group
                  .and_then(|gid| app.tab_groups.get(&gid).map(|g| group_color_egui(g.color)));
                let (rect, resp, action) = tab_ui(
                  ui,
                  motion,
                  tab,
                  is_active,
                  close_t.is_none(),
                  can_close_tabs,
                  tab_width,
                  favicon_tex,
                  &mut app.chrome,
                  focus_ring,
                  group_border,
                  close_t,
                );
                (rect, Some(resp), action)
              };
              layout_tab_rects.push((tab_id, tab_rect));
              if is_active {
                active_tab_rect = Some(tab_rect);
                active_tab_is_pinned = false;
              }
              // Keep the active tab visible when switching tabs via keyboard, tab search, or other
              // non-pointer interactions. Avoid fighting user scrolling: only scroll when the
              // active tab actually changed.
              if is_active && active_changed {
                if let Some(tab_response) = &tab_response {
                  tab_response.scroll_to_me(Some(egui::Align::Center));
                }
              }
              #[cfg(test)]
              tab_rects_for_test.push(tab_rect);

              unpinned_items_for_snapshot.push(TabStripItemKey::Tab(tab_id));
              item_pos += 1;

              // Drag rect lists are built lazily (only when dragging).
              let is_close_action = maybe_action
                .as_ref()
                .is_some_and(|action| matches!(action, ChromeAction::CloseTab(_)));
              if !is_close_action {
                if let Some(tab_response) = &tab_response {
                  if tab_response.drag_started() && app.chrome.dragging_tab_id.is_none() {
                    app.chrome.dragging_tab_id = Some(tab_id);
                    let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                    app.chrome.drag_start_pointer_pos = pointer_pos.map(|p| (p.x, p.y));
                    app.chrome.drag_start_tab_rect = Some((
                      tab_rect.min.x,
                      tab_rect.min.y,
                      tab_rect.max.x,
                      tab_rect.max.y,
                    ));
                    app.chrome.tab_drag_session = app.chrome.tab_drag_session.wrapping_add(1);
                  }
                }
              }
              if app.chrome.dragging_tab_id == Some(tab_id) {
                dragged_tab_rect = Some(tab_rect);
              }

              if let Some(action) = maybe_action {
                actions.push(action);
              }

              prev_kind = GapKind::Normal;
              prev_gap_scale = close_progress.contains_key(&tab_id).then_some(close_scale);

              idx += 1;
            }

            // Placeholder at end (pinning the last unpinned tab).
            if source_placeholder_index.is_some() && !inserted_source_placeholder {
              add_gap(
                ui,
                &mut first_item,
                prev_kind,
                prev_gap_scale,
                GapKind::SourcePlaceholder,
                true,
              );
              let w = source_placeholder_from_w * (1.0 - pin_t);
              let (_, rect) = ui.allocate_space(Vec2::new(w, TAB_HEIGHT));
              #[cfg(test)]
              tab_rects_for_test.push(rect);
            }

            if end_spacer_x > 0.0 {
              ui.add_space(end_spacer_x);
            }
          });
        });
      let mut scroll_state = scroll_output.state;
      scroll_offset_x = scroll_state.offset.x;
      unpinned_max_scroll_x =
        (scroll_output.content_size.x - scroll_output.inner_rect.width()).max(0.0);
      let unpinned_scroll_rect = scroll_output.inner_rect;
      unpinned_scroll_viewport_rect = Some(unpinned_scroll_rect);
      if restore_scroll_delta.is_some() || restore_wheel_deltas.is_some() {
        let restore_scroll_delta = restore_scroll_delta.take();
        let restore_wheel_deltas = restore_wheel_deltas.take();
        unpinned_ui.ctx().input_mut(move |i| {
          if let Some(scroll_delta) = restore_scroll_delta {
            i.scroll_delta = scroll_delta;
          }
          if let Some(wheel_deltas) = restore_wheel_deltas {
            let mut deltas = wheel_deltas.into_iter();
            for event in &mut i.events {
              if let egui::Event::MouseWheel { delta, .. } = event {
                if let Some(saved) = deltas.next() {
                  *delta = saved;
                }
              }
            }
          }
        });
      }

      // Keep our "desired scroll" state in sync with egui's actual scroll offset so we don't fight
      // user scrolling (we only override when clamping due to content shrink).
      unpinned_ui
        .ctx()
        .data_mut(|d| d.insert_temp(desired_scroll_id, scroll_offset_x));

      // Use the scroll area's actual widget id for programmatic state updates, rather than
      // assuming how `id_source` is transformed internally by egui.
      unpinned_ui
        .ctx()
        .data_mut(|d| d.insert_temp(scroll_state_id_key, scroll_output.id));
      let scroll_state_id = scroll_output.id;

      // While dragging an unpinned tab, auto-scroll the overflowing scroll area when the pointer is
      // near the left/right edge of the unpinned viewport (standard browser UX).
      if unpinned_max_scroll_x > 0.5 {
        if let (Some(dragging_tab_id), Some(pointer_pos)) = (
          app.chrome.dragging_tab_id,
          ui.input(|i| i.pointer.interact_pos()),
        ) {
          let dragging_is_unpinned = app.tab(dragging_tab_id).is_some_and(|tab| !tab.pinned);
          if dragging_is_unpinned {
            let dt = ui.ctx().input(|i| i.stable_dt).clamp(0.0, 0.1);
            let delta_x = drag_autoscroll_delta_x(pointer_pos, unpinned_scroll_rect, dt);
            if delta_x != 0.0 {
              let prev = scroll_state.offset.x;
              let next = (prev + delta_x).clamp(0.0, unpinned_max_scroll_x);
              if (next - prev).abs() > 0.01 {
                scroll_state.offset.x = next;
                scroll_state.store(ui.ctx(), scroll_state_id);
                // Keep our "desired scroll" state in sync with programmatic scrolling so the
                // clamp-animation logic (used when content shrinks) sees the updated offset.
                unpinned_ui
                  .ctx()
                  .data_mut(|d| d.insert_temp(desired_scroll_id, next));
                ui.ctx().request_repaint();
              }
            }
          }
        }
      }
    }
  }

  // Visual separator between pinned and unpinned tabs.
  if segment_gap > 0.0 && pinned_viewport_rect.width() > 0.0 && unpinned_viewport_rect.width() > 0.0
  {
    let x = pinned_viewport_rect.max.x + segment_gap * 0.5;
    let y0 = tabs_rect.top() + 6.0;
    let y1 = tabs_rect.bottom() - 6.0;
    let stroke = Stroke::new(
      1.0,
      with_alpha(ui.visuals().widgets.inactive.bg_stroke.color, 0.6),
    );
    ui.painter()
      .with_clip_rect(tabs_rect)
      .line_segment([Pos2::new(x, y0), Pos2::new(x, y1)], stroke);
  }

  // Animate pin/unpin transitions using a "ghost" tab that morphs between the old and new layout
  // positions, while the underlying state change (pinned flag + ordering) remains immediate.
  let mut moving_tab_ghost_rect: Option<Rect> = None;
  if let (Some(anim), Some(dest_rect)) = (pin_anim_render, ghost_dest_anchor_rect) {
    if let Some(tab) = app.tab(anim.tab_id) {
      let ghost_rect = lerp_rect(anim.from_rect, dest_rect, pin_t);
      moving_tab_ghost_rect = Some(ghost_rect);
      let favicon_tex = favicon_for_tab(anim.tab_id);
      let is_active = active_id == Some(anim.tab_id);
      let mut ghost_ui = ui.child_ui(tabs_rect, egui::Layout::left_to_right(egui::Align::Center));
      ghost_ui.set_clip_rect(tabs_rect);
      paint_tab_pin_ghost(
        &ghost_ui,
        motion,
        tab,
        ghost_rect,
        favicon_tex,
        is_active,
        anim.kind,
        pin_t,
      );
    }
  }

  // Edge fades: scrollbars are hidden, so use subtle fades as the overflow affordance.
  if pinned_count > 0 && pinned_viewport_rect.width() > 0.0 {
    let viewport_rect = pinned_scroll_viewport_rect.unwrap_or(pinned_viewport_rect);
    paint_scroll_edge_fades(
      ui,
      viewport_rect,
      pinned_scroll_offset_x,
      pinned_max_scroll_x,
    );
  }
  if unpinned_viewport_rect.width() > 0.0 {
    let viewport_rect = unpinned_scroll_viewport_rect.unwrap_or(unpinned_viewport_rect);
    paint_scroll_edge_fades(ui, viewport_rect, scroll_offset_x, unpinned_max_scroll_x);
  }

  // Micro-interaction: animate the active tab underline position/width.
  // If the active tab is currently pinning/unpinning, draw the underline directly under the moving
  // ghost rect (so it doesn't lag behind or disappear while the real widget is suppressed).
  if let Some(active_rect) = pin_anim_render
    .filter(|anim| active_id == Some(anim.tab_id))
    .and_then(|_| moving_tab_ghost_rect)
  {
    let width = (active_rect.width() - 20.0).max(0.0);
    let x0 = active_rect.center().x - width * 0.5;
    let x1 = active_rect.center().x + width * 0.5;
    let y = active_rect.max.y - ACTIVE_UNDERLINE_HEIGHT * 0.5;
    ui.painter().with_clip_rect(tabs_rect).line_segment(
      [Pos2::new(x0, y), Pos2::new(x1, y)],
      Stroke::new(ACTIVE_UNDERLINE_HEIGHT, ui.visuals().selection.stroke.color),
    );
  } else if let Some(active_rect) = active_tab_rect {
    let pinned_scroll_rect = pinned_scroll_viewport_rect.unwrap_or(pinned_viewport_rect);
    let unpinned_scroll_rect = unpinned_scroll_viewport_rect.unwrap_or(unpinned_viewport_rect);
    let underline_id = ui.make_persistent_id("tab_strip_active_underline");
    let pinned_offset = unpinned_scroll_rect.min.x - tabs_rect.min.x;
    // Animate in a unified content coordinate space so the underline tracks scroll (unpinned tabs)
    // without lag, while still supporting pinned tabs which live in a separate scroll area.
    let target_center_content_x = if active_tab_is_pinned {
      // Pinned tabs are inside their own horizontal scroll area. Convert the active tab's *screen*
      // position back into *pinned-content* space so `animate_f32` doesn't "chase" the scroll and
      // visibly lag behind when the pinned strip is scrolled (wheel or drag autoscroll).
      (active_rect.center().x - pinned_scroll_rect.min.x) + pinned_scroll_offset_x
    } else {
      pinned_offset + (active_rect.center().x - unpinned_scroll_rect.min.x + scroll_offset_x)
    };
    let target_width = (active_rect.width() - 20.0).max(0.0);
    let center_content_x = motion.animate_f32(
      ui.ctx(),
      underline_id.with("x"),
      target_center_content_x,
      motion.durations.tab_underline,
    );
    let width = motion.animate_f32(
      ui.ctx(),
      underline_id.with("w"),
      target_width,
      motion.durations.tab_underline,
    );

    let center_screen_x = if active_tab_is_pinned {
      // Convert back to screen space for painting, applying the current scroll offset so the
      // underline tracks pinned scrolling without lag.
      pinned_scroll_rect.min.x + (center_content_x - pinned_scroll_offset_x)
    } else {
      unpinned_scroll_rect.min.x + (center_content_x - pinned_offset) - scroll_offset_x
    };
    let x0 = center_screen_x - width * 0.5;
    let x1 = center_screen_x + width * 0.5;
    let y = active_rect.max.y - ACTIVE_UNDERLINE_HEIGHT * 0.5;
    ui.painter().with_clip_rect(tabs_rect).line_segment(
      [Pos2::new(x0, y), Pos2::new(x1, y)],
      Stroke::new(ACTIVE_UNDERLINE_HEIGHT, ui.visuals().selection.stroke.color),
    );
  }

  // Drag-to-reorder uses ordered lists of the *visible* tab rects. When no drag is active, avoid
  // allocating and populating these potentially-large vectors (hundreds of tabs) every frame.
  if let Some(dragging_tab_id) = app.chrome.dragging_tab_id {
    let dragging_is_pinned = app.tab(dragging_tab_id).map(|t| t.pinned).unwrap_or(false);
    let split = pinned_tab_rects_end.min(layout_tab_rects.len());
    let (pinned_rects, unpinned_rects) = layout_tab_rects.split_at(split);
    let filter_closing = !close_progress.is_empty();
    if dragging_is_pinned {
      pinned_tab_rects_for_drag.reserve(pinned_rects.len());
      if filter_closing {
        for (tab_id, rect) in pinned_rects.iter().copied() {
          if close_progress.contains_key(&tab_id) {
            continue;
          }
          pinned_tab_rects_for_drag.push((tab_id, rect));
        }
      } else {
        pinned_tab_rects_for_drag.extend(pinned_rects.iter().copied());
      }
    } else {
      unpinned_tab_rects_for_drag.reserve(unpinned_rects.len());
      if filter_closing {
        for (tab_id, rect) in unpinned_rects.iter().copied() {
          if close_progress.contains_key(&tab_id) {
            continue;
          }
          unpinned_tab_rects_for_drag.push((tab_id, rect));
        }
      } else {
        unpinned_tab_rects_for_drag.extend(unpinned_rects.iter().copied());
      }
    }
  }

  // Persist the layout snapshot for pin/unpin animations (and reuse it as per-frame scratch).
  pinned_tabs_for_snapshot.extend(app.tabs.iter().take(pinned_len).map(|t| t.id));
  layout_snapshot.pinned_tabs = pinned_tabs_for_snapshot;
  layout_snapshot.unpinned_items = unpinned_items_for_snapshot;
  layout_snapshot.tab_rects = layout_tab_rects;
  layout_snapshot.unpinned_tab_width = sizing.tab_width;
  layout_snapshot.scroll_offset_x = scroll_offset_x;
  layout_snapshot.pinned_count = pinned_count;
  layout_snapshot.unpinned_count = unpinned_count;

  let perf_sample = if perf_enabled {
    let elapsed_us = perf_start
      .as_ref()
      .map(|start| start.elapsed().as_micros())
      .unwrap_or(0);
    Some(TabStripPerfSample {
      frame_us: elapsed_us.min(u64::MAX as u128) as u64,
      tab_count,
      pinned_count,
      unpinned_count,
      pinned_tabs_cap: layout_snapshot.pinned_tabs.capacity(),
      unpinned_items_cap: layout_snapshot.unpinned_items.capacity(),
      tab_rects_cap: layout_snapshot.tab_rects.capacity(),
    })
  } else {
    None
  };

  ctx.data_mut(|d| {
    if let Some(sample) = perf_sample {
      d.insert_temp(egui::Id::new("tab_strip_perf_sample"), sample);
    }
    d.insert_temp(group_chip_width_cache_key, group_chip_width_cache);
    d.insert_temp(snapshot_key, Some(layout_snapshot));
    d.insert_temp(anim_key, pin_anim_render.cloned());
  });

  // Keep the drag-lift animation state "armed" across drag sessions.
  //
  // The tab drag ghost only exists while `dragging_tab_id` is `Some`, so we have to "prime" the
  // animation state on drag start (set `t=0`) and also drive it back toward rest on drag end.
  let drag_lift_state_id = ui.make_persistent_id("tab_strip_drag_lift_state");
  let last_dragged_id_key = drag_lift_state_id.with("last_dragged_tab_id");
  let last_dragged_id = ctx
    .data(|d| d.get_temp::<Option<TabId>>(last_dragged_id_key))
    .unwrap_or(None);
  let dragging_tab_id = app.chrome.dragging_tab_id;
  if last_dragged_id != dragging_tab_id {
    // Drag start: prime the new tab's animation state at `t=0` so the lift-in animates even the
    // first time a tab is dragged (egui otherwise initializes animations at their target value).
    if let Some(new_id) = dragging_tab_id {
      let _ = motion.animate_bool(
        &ctx,
        egui::Id::new(("tab_drag_lift", new_id)),
        false,
        motion.durations.tab_drag_lift,
      );
    }

    // Drag end (or drag-switch): drive the previous tab back toward rest so subsequent drags lift
    // in again.
    if let Some(prev_id) = last_dragged_id {
      let _ = motion.animate_bool(
        &ctx,
        egui::Id::new(("tab_drag_lift", prev_id)),
        false,
        motion.durations.tab_drag_lift,
      );
    }

    ctx.data_mut(|d| d.insert_temp(last_dragged_id_key, dragging_tab_id));
  }

  // Drag-to-reorder: apply the reorder while dragging, but render the dragged tab as a floating
  // preview so it feels "picked up".
  if let (Some(dragging_tab_id), Some(pos)) = (
    app.chrome.dragging_tab_id,
    ui.input(|i| i.pointer.interact_pos()),
  ) {
    // While dragging, ensure we keep repainting so hover/indicator stays responsive even if the
    // host winit loop relies on egui repaint requests.
    ui.ctx().request_repaint();
    let dragging_is_pinned = app.tab(dragging_tab_id).map(|t| t.pinned).unwrap_or(false);

    let pinned_clip_rect = pinned_scroll_viewport_rect.unwrap_or(pinned_viewport_rect);
    let unpinned_clip_rect = unpinned_scroll_viewport_rect.unwrap_or(unpinned_viewport_rect);
    let (tab_rects_for_drag, group_start_index, group_clip_rect) = if dragging_is_pinned {
      (&pinned_tab_rects_for_drag, 0usize, pinned_clip_rect)
    } else {
      (
        &unpinned_tab_rects_for_drag,
        pinned_count,
        unpinned_clip_rect,
      )
    };

    // Used for placeholder + preview styling.
    let dragged_group_color = app
      .tab(dragging_tab_id)
      .and_then(|t| t.group)
      .and_then(|gid| app.tab_groups.get(&gid))
      .map(|g| group_color_egui(g.color));

    let drag_anim_key = (dragging_tab_id, app.chrome.tab_drag_session);

    let mut insertion_index: Option<usize> = None;
    let mut target_index: Option<usize> = None;
    let mut insertion_changed = false;

    if tab_rects_for_drag.len() >= 2 {
      // Determine the insertion point by comparing the pointer x coordinate against the centers of
      // each *other* tab.
      let idx = compute_tab_insertion_index(pos.x, tab_rects_for_drag, dragging_tab_id);
      insertion_index = Some(idx);

      // Map the insertion point (computed from visible tab rects) back to an index into
      // `BrowserAppState.tabs` so group invariants are preserved even with collapsed groups.
      let src_idx = app.tabs.iter().position(|t| t.id == dragging_tab_id);
      let before_id = {
        let mut count = 0usize;
        let mut out = None::<TabId>;
        for (tab_id, _) in tab_rects_for_drag {
          if *tab_id == dragging_tab_id {
            continue;
          }
          if count == idx {
            out = Some(*tab_id);
            break;
          }
          count += 1;
        }
        out
      };
      let mut dst = if let Some(before_id) = before_id {
        app
          .tabs
          .iter()
          .position(|t| t.id == before_id)
          .unwrap_or(group_start_index)
      } else {
        let last_id = tab_rects_for_drag
          .iter()
          .rev()
          .find_map(|(tab_id, _)| (*tab_id != dragging_tab_id).then_some(*tab_id));
        last_id
          .and_then(|id| app.tabs.iter().position(|t| t.id == id).map(|idx| idx + 1))
          .unwrap_or(group_start_index)
      };
      if let Some(src_idx) = src_idx {
        if src_idx < dst {
          dst = dst.saturating_sub(1);
        }
      }
      insertion_changed = app.chrome.drag_target_index != Some(dst);
      target_index = Some(dst);
    }

    // Placeholder gap (drawn over the in-flow slot so the tab reads as detached even on the first
    // drag frame).
    if let Some(rect) = dragged_tab_rect {
      let visuals = ui.visuals().clone();
      let painter = ui.painter().with_clip_rect(group_clip_rect);
      painter.rect_filled(rect, visuals.widgets.inactive.rounding, visuals.panel_fill);
      let pulse_target = if motion_enabled && insertion_changed {
        1.0
      } else {
        0.0
      };
      let pulse_t = motion.animate_f32(
        ui.ctx(),
        egui::Id::new("tab_strip_drag_gap_pulse").with(drag_anim_key),
        pulse_target,
        motion.durations.hover_fade,
      );
      paint_drag_placeholder(&painter, rect, &visuals, dragged_group_color, pulse_t);
    }

    // Drop indicator.
    if let Some(insertion_index) = insertion_index {
      let tab_strip_rect = tab_rects_for_drag
        .iter()
        .map(|(_, rect)| *rect)
        .reduce(|a, b| a.union(b))
        .unwrap_or_else(|| Rect::NOTHING);

      let drop_x = if insertion_index == 0 {
        tab_rects_for_drag
          .first()
          .map(|(_, rect)| rect.left())
          .unwrap_or(tab_strip_rect.left())
      } else if insertion_index >= tab_rects_for_drag.len() - 1 {
        tab_rects_for_drag
          .last()
          .map(|(_, rect)| rect.right())
          .unwrap_or(tab_strip_rect.right())
      } else {
        let mut count = 0usize;
        let mut x = tab_strip_rect.left();
        for (tab_id, rect) in tab_rects_for_drag {
          if *tab_id == dragging_tab_id {
            continue;
          }
          if count == insertion_index {
            x = rect.left();
            break;
          }
          count += 1;
        }
        x
      };

      let drop_id = egui::Id::new("tab_strip_drop_indicator").with(drag_anim_key);
      // Animate the indicator in the scroll area's *content* coordinate space so it tracks
      // horizontal scrolling without lag (similar to the active-tab underline).
      let segment_scroll_offset_x = if dragging_is_pinned {
        pinned_scroll_offset_x
      } else {
        scroll_offset_x
      };
      let drop_x_content = (drop_x - group_clip_rect.min.x) + segment_scroll_offset_x;
      let drop_x_content = motion.animate_f32(
        ui.ctx(),
        drop_id.with("x"),
        drop_x_content,
        motion.durations.tab_drag_indicator,
      );
      let drop_x = group_clip_rect.min.x + (drop_x_content - segment_scroll_offset_x);
      let target_alpha = if motion_enabled && insertion_changed {
        1.0
      } else if motion_enabled {
        0.7
      } else {
        1.0
      };
      let alpha = motion.animate_f32(
        ui.ctx(),
        drop_id.with("a"),
        target_alpha,
        motion.durations.tab_drag_indicator,
      );

      let y1 = tab_strip_rect.top() + 1.0;
      let y2 = tab_strip_rect.bottom() - 1.0;

      // Use the tab group's accent color (when present) and fall back to the global selection
      // stroke (accent) rather than the generic widget border. This keeps the insertion indicator
      // consistent with the active-tab underline and other chrome highlights.
      let indicator_color = dragged_group_color.unwrap_or(ui.visuals().selection.stroke.color);

      // Add a subtle high-contrast "halo" so even dark group colors remain visible in dark themes
      // (and vice versa).
      let halo_color = contrast_bw(ui.visuals().panel_fill);
      let halo_stroke = Stroke::new(3.0, with_alpha(halo_color, alpha * 0.55));
      let inner_stroke = Stroke::new(2.0, with_alpha(indicator_color, alpha));

      let painter = ui.painter().with_clip_rect(group_clip_rect);
      painter.line_segment([Pos2::new(drop_x, y1), Pos2::new(drop_x, y2)], halo_stroke);
      painter.line_segment([Pos2::new(drop_x, y1), Pos2::new(drop_x, y2)], inner_stroke);
    }

    // Floating preview.
    let preview_size = app
      .chrome
      .drag_start_tab_rect
      .map(|r| rect_from_points_tuple(r).size())
      .or_else(|| dragged_tab_rect.map(|r| r.size()))
      .unwrap_or_else(|| {
        Vec2::new(
          if dragging_is_pinned {
            PINNED_TAB_WIDTH
          } else {
            sizing.tab_width
          },
          TAB_HEIGHT,
        )
      });

    let delta = app
      .chrome
      .drag_start_pointer_pos
      .map(|(x, y)| pos - Pos2::new(x, y))
      .unwrap_or_default();
    let mut preview_rect = app
      .chrome
      .drag_start_tab_rect
      .map(rect_from_points_tuple)
      .or(dragged_tab_rect)
      .map(|rect| rect.translate(delta))
      .unwrap_or_else(|| Rect::from_center_size(pos, preview_size));

    // Animate the lift/scale-in so the dragged tab feels like it's being "picked up". When reduced
    // motion is enabled this snaps to the final state.
    let lift_id = egui::Id::new(("tab_drag_lift", dragging_tab_id));
    let lift_t = motion.animate_bool(&ctx, lift_id, true, motion.durations.tab_drag_lift);
    let lift = Vec2::new(0.0, -DRAG_PREVIEW_LIFT_Y * lift_t);
    preview_rect = preview_rect.translate(lift);
    let scale = 1.0 + (DRAG_PREVIEW_SCALE - 1.0) * lift_t;
    preview_rect =
      Rect::from_center_size(preview_rect.center(), preview_rect.size() * scale.max(0.01));

    let preview_favicon_tex = favicon_for_tab(dragging_tab_id);
    let preview_is_active = active_id == Some(dragging_tab_id);

    if let Some(tab) = app.tab(dragging_tab_id) {
      let preview_id = egui::Id::new("tab_strip_drag_preview");
      egui::Area::new(preview_id)
        .order(egui::Order::Foreground)
        .fixed_pos(preview_rect.min)
        .interactable(false)
        .show(ui.ctx(), |ui| {
          ui.set_clip_rect(ui.ctx().screen_rect());
          ui.allocate_space(preview_rect.size());
          let visuals = ui.style().visuals.clone();
          let rounding = visuals.widgets.inactive.rounding;
          let mut shadow = visuals.popup_shadow;
          shadow.extrusion *= lift_t;
          shadow.color = with_alpha(shadow.color, lift_t);
          paint_popup_shadow(ui.painter(), preview_rect, rounding, shadow);

          if dragging_is_pinned {
            pinned_tab_preview_ui(
              ui,
              motion,
              tab,
              preview_is_active,
              preview_rect,
              preview_favicon_tex,
            );
          } else {
            unpinned_tab_preview_ui(
              ui,
              motion,
              tab,
              preview_is_active,
              can_close_tabs,
              preview_rect,
              preview_favicon_tex,
              dragged_group_color,
            );
          }
        });
    }

    if let Some(target_index) = target_index {
      // Only apply the reorder when the inferred target index changes, to avoid unnecessary churn
      // (and to keep group inference stable) while the pointer is stationary.
      if insertion_changed {
        app.chrome.drag_target_index = Some(target_index);
        // Apply the reorder immediately while dragging (standard browser behaviour).
        app.drag_reorder_tab(dragging_tab_id, target_index);
      }
    }
  }

  // Optional: animate the drag ghost settling back down after a drop inside the strip.
  //
  // The drag preview is normally only rendered while `dragging_tab_id` is `Some`, so to get a
  // visible lift-out we store the base preview rect at drop time and keep rendering it for the
  // duration of `tab_drag_lift` while animating `t: 1 → 0`.
  if app.chrome.dragging_tab_id.is_none() {
    let lift_out = ctx
      .data(|d| d.get_temp::<Option<TabDragLiftOut>>(drag_lift_out_key))
      .unwrap_or(None);
    if let Some(lift_out) = lift_out {
      let tab_id = lift_out.tab_id;
      let tab = match app.tab(tab_id) {
        Some(tab) => Some(tab),
        None => {
          ctx.data_mut(|d| d.insert_temp(drag_lift_out_key, None::<TabDragLiftOut>));
          None
        }
      };

      if let Some(tab) = tab {
        let lift_id = egui::Id::new(("tab_drag_lift", tab_id));
        let lift_t = motion.animate_bool(&ctx, lift_id, false, motion.durations.tab_drag_lift);

        // Animation finished (or reduced motion): drop the transient state and skip drawing.
        if lift_t <= 1e-3 {
          ctx.data_mut(|d| d.insert_temp(drag_lift_out_key, None::<TabDragLiftOut>));
        } else {
          // Keep repainting while the lift-out is in progress.
          if motion.enabled && ctx.style().animation_time > 0.0 {
            ctx.request_repaint();
          }

          let lift = Vec2::new(0.0, -DRAG_PREVIEW_LIFT_Y * lift_t);
          let mut preview_rect = lift_out.base_rect.translate(lift);
          let scale = 1.0 + (DRAG_PREVIEW_SCALE - 1.0) * lift_t;
          preview_rect =
            Rect::from_center_size(preview_rect.center(), preview_rect.size() * scale.max(0.01));

          let preview_favicon_tex = favicon_for_tab(tab_id);
          let preview_is_active = active_id == Some(tab_id);
          let preview_id = egui::Id::new("tab_strip_drag_preview");
          egui::Area::new(preview_id)
            .order(egui::Order::Foreground)
            .fixed_pos(preview_rect.min)
            .interactable(false)
            .show(&ctx, |ui| {
              ui.set_clip_rect(ui.ctx().screen_rect());
              ui.allocate_space(preview_rect.size());
              let visuals = ui.style().visuals.clone();
              let rounding = visuals.widgets.inactive.rounding;
              let mut shadow = visuals.popup_shadow;
              shadow.extrusion *= lift_t;
              shadow.color = with_alpha(shadow.color, lift_t);
              paint_popup_shadow(ui.painter(), preview_rect, rounding, shadow);

              if tab.pinned {
                pinned_tab_preview_ui(
                  ui,
                  motion,
                  tab,
                  preview_is_active,
                  preview_rect,
                  preview_favicon_tex,
                );
              } else {
                let group_color = tab
                  .group
                  .and_then(|gid| app.tab_groups.get(&gid))
                  .map(|g| group_color_egui(g.color));
                unpinned_tab_preview_ui(
                  ui,
                  motion,
                  tab,
                  preview_is_active,
                  can_close_tabs,
                  preview_rect,
                  preview_favicon_tex,
                  group_color,
                );
              }
            });
        }
      }
    }
  } else {
    // If the user starts another drag while a lift-out is in progress, kill the old ghost
    // immediately (avoid overlapping drag previews).
    let has_lift_out = ctx
      .data(|d| d.get_temp::<Option<TabDragLiftOut>>(drag_lift_out_key))
      .unwrap_or(None)
      .is_some();
    if has_lift_out {
      ctx.data_mut(|d| d.insert_temp(drag_lift_out_key, None::<TabDragLiftOut>));
    }
  }

  // Drag-to-detach: dragging far enough away from the tab strip (or releasing outside the strip)
  // should detach the tab into a new window.
  if let Some(dragging_tab_id) = app.chrome.dragging_tab_id {
    let detach_on_drag = ui.input(|i| {
      i.pointer
        .interact_pos()
        .is_some_and(|pos| !strip_rect.expand(TAB_DETACH_DRAG_THRESHOLD).contains(pos))
    });

    let release_pos = ui.input(|i| {
      i.events.iter().find_map(|event| match event {
        egui::Event::PointerButton {
          pos,
          button: egui::PointerButton::Primary,
          pressed: false,
          ..
        } => Some(*pos),
        _ => None,
      })
    });
    let detach_on_release = release_pos.is_some_and(|pos| !strip_rect.contains(pos));

    if detach_on_drag || detach_on_release {
      actions.push(ChromeAction::DetachTab(dragging_tab_id));
      // Don't animate a lift-out in this window when detaching.
      ctx.data_mut(|d| d.insert_temp(drag_lift_out_key, None::<TabDragLiftOut>));
      app.chrome.clear_tab_drag();
    } else if let Some(pos) = release_pos {
      // Regular drop inside the strip: keep the ghost alive briefly so it can settle back down.
      if motion_enabled && motion.durations.tab_drag_lift > 0.0 {
        let dragging_is_pinned = app.tab(dragging_tab_id).is_some_and(|tab| tab.pinned);
        let preview_size = app
          .chrome
          .drag_start_tab_rect
          .map(|r| rect_from_points_tuple(r).size())
          .unwrap_or_else(|| {
            Vec2::new(
              if dragging_is_pinned {
                PINNED_TAB_WIDTH
              } else {
                sizing.tab_width
              },
              TAB_HEIGHT,
            )
          });
        let delta = app
          .chrome
          .drag_start_pointer_pos
          .map(|(x, y)| pos - Pos2::new(x, y))
          .unwrap_or_default();
        let base_rect = app
          .chrome
          .drag_start_tab_rect
          .map(rect_from_points_tuple)
          .map(|rect| rect.translate(delta))
          .unwrap_or_else(|| Rect::from_center_size(pos, preview_size));
        ctx.data_mut(|d| {
          d.insert_temp(
            drag_lift_out_key,
            Some(TabDragLiftOut {
              tab_id: dragging_tab_id,
              base_rect,
            }),
          );
        });
      } else {
        ctx.data_mut(|d| d.insert_temp(drag_lift_out_key, None::<TabDragLiftOut>));
      }

      app.chrome.clear_tab_drag();
    }
  }

  for op in ops.drain(..) {
    match op {
      TabStripOp::ToggleGroupCollapsed(group_id) => {
        app.toggle_group_collapsed(group_id);
      }
      TabStripOp::Ungroup(group_id) => {
        app.ungroup(group_id);
      }
      TabStripOp::SetGroupColor(group_id, color) => {
        app.set_group_color(group_id, color);
      }
    }
  }

  // New tab button stays visible even when the tab list overflows.
  let new_tab_resp = ui
    .allocate_ui_at_rect(button_rect, |ui| {
      ui.spacing_mut().interact_size = Vec2::splat(button_rect.height());
      ui.spacing_mut().icon_width = ICON_SIZE;
      icon_button_with_id(
        ui,
        egui::Id::new("chrome_tab_strip_new_tab_button"),
        BrowserIcon::NewTab,
        "New tab (Ctrl/Cmd+T)",
        true,
      )
    })
    .inner;
  #[cfg(test)]
  ui.ctx()
    .data_mut(|d| d.insert_temp(egui::Id::new("test_tab_strip_new_tab_id"), new_tab_resp.id));
  new_tab_resp.widget_info(|| {
    egui::WidgetInfo::labeled(egui::WidgetType::Button, BrowserIcon::NewTab.a11y_label())
  });
  #[cfg(test)]
  super::store_test_id(
    ui.ctx(),
    "chrome_tab_strip_new_tab_button_id",
    new_tab_resp.id,
  );
  if new_tab_resp.clicked() {
    actions.push(ChromeAction::NewTab);
  }

  if any_closing_tab_animating && motion_enabled {
    // Keep repainting while at least one tab is actively animating closed.
    ui.ctx().request_repaint_after(Duration::from_millis(16));
  }

  #[cfg(test)]
  {
    store_test_layout(ui.ctx(), strip_rect, tab_rects_for_test);
    ui.ctx().data_mut(|d| {
      d.insert_temp(
        egui::Id::new("test_tab_strip_unpinned_scroll_offset_x"),
        scroll_offset_x,
      );
      d.insert_temp(
        egui::Id::new("test_tab_strip_unpinned_scroll_viewport_rect"),
        unpinned_scroll_viewport_rect,
      );
      d.insert_temp(
        egui::Id::new("test_tab_strip_pinned_scroll_offset_x"),
        pinned_scroll_offset_x,
      );
      d.insert_temp(
        egui::Id::new("test_tab_strip_pinned_scroll_viewport_rect"),
        pinned_scroll_viewport_rect,
      );
    });
  }

  actions
}

#[cfg(test)]
fn store_test_layout(ctx: &egui::Context, strip_rect: Rect, tab_rects: Vec<Rect>) {
  ctx.data_mut(|d| {
    d.insert_temp(egui::Id::new("test_tab_strip_rect"), strip_rect);
    d.insert_temp(egui::Id::new("test_tab_strip_tab_rects"), tab_rects);
  });
}

#[cfg(test)]
fn store_test_close_id(ctx: &egui::Context, tab_id: TabId, close_id: egui::Id) {
  let key = egui::Id::new("test_tab_strip_close_ids");
  ctx.data_mut(|d| {
    let mut ids = d
      .get_temp::<Vec<(TabId, egui::Id)>>(key)
      .unwrap_or_default();
    ids.push((tab_id, close_id));
    d.insert_temp(key, ids);
  });
}

#[cfg(test)]
fn store_test_close_rect(ctx: &egui::Context, tab_id: TabId, close_rect: Rect) {
  let key = egui::Id::new("test_tab_strip_close_rects");
  ctx.data_mut(|d| {
    let mut rects = d.get_temp::<Vec<(TabId, Rect)>>(key).unwrap_or_default();
    rects.push((tab_id, close_rect));
    d.insert_temp(key, rects);
  });
}

#[cfg(test)]
pub(super) fn load_test_layout(ctx: &egui::Context) -> Option<(Rect, Vec<Rect>)> {
  ctx.data(|d| {
    let strip = d.get_temp::<Rect>(egui::Id::new("test_tab_strip_rect"))?;
    let tabs = d.get_temp::<Vec<Rect>>(egui::Id::new("test_tab_strip_tab_rects"))?;
    Some((strip, tabs))
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::a11y_test_util;

  fn begin_frame(ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn key_press(key: egui::Key) -> egui::Event {
    egui::Event::Key {
      key,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers::default(),
    }
  }

  fn shift_tab_press() -> egui::Event {
    egui::Event::Key {
      key: egui::Key::Tab,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers {
        shift: true,
        ..Default::default()
      },
    }
  }

  fn render_tab_strip(ctx: &egui::Context, app: &mut BrowserAppState) -> Vec<ChromeAction> {
    egui::CentralPanel::default()
      .show(ctx, |ui| {
        let mut favicon_for_tab = |_| None;
        tab_strip_ui(
          ui,
          app,
          &mut favicon_for_tab,
          UiMotion::new(false),
          FocusRingStyle {
            stroke: egui::Stroke::new(1.0, egui::Color32::WHITE),
            expand: 0.0,
            rounding: egui::Rounding::same(0.0),
          },
        )
      })
      .inner
  }

  fn tab_strip_close_ids(ctx: &egui::Context) -> HashMap<TabId, egui::Id> {
    let close_ids = ctx
      .data(|d| d.get_temp::<Vec<(TabId, egui::Id)>>(egui::Id::new("test_tab_strip_close_ids")))
      .expect("expected test_tab_strip_close_ids");
    let mut map = HashMap::new();
    for (tab_id, close_id) in close_ids {
      map.insert(tab_id, close_id);
    }
    map
  }

  fn tab_strip_close_rects(ctx: &egui::Context) -> HashMap<TabId, Rect> {
    let close_rects = ctx
      .data(|d| d.get_temp::<Vec<(TabId, Rect)>>(egui::Id::new("test_tab_strip_close_rects")))
      .expect("expected test_tab_strip_close_rects");
    let mut map = HashMap::new();
    for (tab_id, rect) in close_rects {
      map.insert(tab_id, rect);
    }
    map
  }

  fn tab_strip_new_tab_id(ctx: &egui::Context) -> egui::Id {
    ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new("test_tab_strip_new_tab_id")))
      .expect("expected test_tab_strip_new_tab_id")
  }

  fn assert_tab_traversal_forward(
    ctx: &egui::Context,
    app: &mut BrowserAppState,
    order: &[egui::Id],
  ) {
    assert!(!order.is_empty(), "expected non-empty traversal order");
    // Frame 1: focus the first widget.
    ctx.memory_mut(|mem| mem.request_focus(order[0]));
    begin_frame(ctx, Vec::new());
    let _ = render_tab_strip(ctx, app);
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(order[0])),
      "expected initial focus on {:#?}",
      order[0]
    );

    // Subsequent frames: press Tab and ensure focus advances in the expected order.
    for (idx, expected) in order.iter().enumerate().skip(1) {
      begin_frame(ctx, vec![key_press(egui::Key::Tab)]);
      let _ = render_tab_strip(ctx, app);
      let _ = ctx.end_frame();

      let focused = order
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));
      assert_eq!(
        focused,
        Some(*expected),
        "unexpected focus after Tab step {idx}; expected {expected:?}, got {focused:?}"
      );
    }
  }

  fn assert_tab_traversal_reverse(
    ctx: &egui::Context,
    app: &mut BrowserAppState,
    order: &[egui::Id],
  ) {
    assert!(!order.is_empty(), "expected non-empty traversal order");
    let reverse: Vec<_> = order.iter().rev().copied().collect();

    // Frame 1: focus the last widget.
    ctx.memory_mut(|mem| mem.request_focus(reverse[0]));
    begin_frame(ctx, Vec::new());
    let _ = render_tab_strip(ctx, app);
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(reverse[0])),
      "expected initial focus on {:#?}",
      reverse[0]
    );

    // Subsequent frames: press Shift+Tab and ensure focus moves in reverse order.
    for (idx, expected) in reverse.iter().enumerate().skip(1) {
      begin_frame(ctx, vec![shift_tab_press()]);
      let _ = render_tab_strip(ctx, app);
      let _ = ctx.end_frame();

      let focused = order
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));
      assert_eq!(
        focused,
        Some(*expected),
        "unexpected focus after Shift+Tab step {idx}; expected {expected:?}, got {focused:?}"
      );
    }
  }

  #[test]
  fn group_chip_context_menu_opens_via_shift_f10_and_focuses_rename() {
    let ctx = egui::Context::default();

    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );
    let group_id = app.create_group_with_tabs(&[tab_a, tab_b]);
    assert_ne!(group_id, TabGroupId(0));

    // Frame 1: render once so the chip exists and we can capture its widget id.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let chip_id = ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new("test_tab_group_chip_id")))
      .expect("expected test_tab_group_chip_id to be stored");

    // Frame 2: focus the chip.
    ctx.memory_mut(|mem| mem.request_focus(chip_id));
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(chip_id)),
      "expected group chip to have focus"
    );

    // Frame 3: inject Shift+F10.
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.time = Some(0.0);
    raw.focused = true;
    raw.modifiers.shift = true;
    raw.events = vec![egui::Event::Key {
      key: egui::Key::F10,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers {
        shift: true,
        ..Default::default()
      },
    }];
    ctx.begin_frame(raw);
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let menu_open = ctx
      .data(|d| d.get_temp::<bool>(egui::Id::new("test_tab_group_context_menu_open")))
      .unwrap_or(false);
    assert!(
      menu_open,
      "expected group context menu to be open after Shift+F10"
    );

    let rename_id = ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new("test_tab_group_rename_id")))
      .expect("expected rename id to be stored");
    assert!(
      ctx.memory(|mem| mem.has_focus(rename_id)),
      "expected focus to move to rename text edit after opening via keyboard"
    );
  }

  #[test]
  fn tab_context_menu_opens_via_shift_f10_when_tab_focused() {
    let ctx = egui::Context::default();
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "https://a.example/".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "https://b.example/".to_string()), false);

    // Frame 1: render once so the tab widgets exist and test layout is stored.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let tab_id = tab_strip_tab_widget_id(tab_a);

    // Frame 2: focus the tab.
    ctx.memory_mut(|mem| mem.request_focus(tab_id));
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(tab_id)),
      "expected tab to have focus"
    );

    // Frame 3: inject Shift+F10.
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.time = Some(0.0);
    raw.focused = true;
    raw.modifiers.shift = true;
    raw.events = vec![egui::Event::Key {
      key: egui::Key::F10,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers {
        shift: true,
        ..Default::default()
      },
    }];
    ctx.begin_frame(raw);
    let actions = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions.is_empty(),
      "expected Shift+F10 to open context menu without emitting tab actions, got {actions:?}"
    );

    let state = app
      .chrome
      .open_tab_context_menu
      .expect("expected tab context menu to be opened after Shift+F10");
    assert_eq!(state.tab_id, tab_a);
    assert_eq!(state.opener_focus, Some(UiFocusToken(tab_a.0)));

    let (_strip_rect, tab_rects) =
      load_test_layout(&ctx).expect("expected test tab strip layout to be stored");
    assert!(
      !tab_rects.is_empty(),
      "expected at least one tab rect to be stored"
    );
    let tab_rect = tab_rects[0];

    let (ax, ay) = state.anchor_points;
    assert!(
      (ax - tab_rect.left()).abs() < 0.5,
      "expected anchor x to be tab_rect.left() ({}), got {ax}",
      tab_rect.left()
    );
    assert!(
      (ay - tab_rect.bottom()).abs() < 0.5,
      "expected anchor y to be tab_rect.bottom() ({}), got {ay}",
      tab_rect.bottom()
    );
  }

  #[test]
  fn focus_traversal_tab_strip_is_left_to_right() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let tab_c = TabId(3);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_c, "https://c.example/".to_string()),
      false,
    );

    let ctx = egui::Context::default();

    // Frame 0: render once to capture deterministic close/new-tab ids.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let close_ids = tab_strip_close_ids(&ctx);
    let new_tab_id = tab_strip_new_tab_id(&ctx);

    let order = vec![
      tab_strip_tab_widget_id(tab_a),
      *close_ids.get(&tab_a).expect("expected close id for tab_a"),
      tab_strip_tab_widget_id(tab_b),
      *close_ids.get(&tab_b).expect("expected close id for tab_b"),
      tab_strip_tab_widget_id(tab_c),
      *close_ids.get(&tab_c).expect("expected close id for tab_c"),
      new_tab_id,
    ];

    assert_tab_traversal_forward(&ctx, &mut app, &order);
  }

  #[test]
  fn focus_traversal_tab_strip_includes_pinned_tabs_before_unpinned() {
    let mut app = BrowserAppState::new();
    let tab_pinned = TabId(1);
    let tab_unpinned = TabId(2);
    let mut pinned = BrowserTabState::new(tab_pinned, "https://pinned.example/".to_string());
    pinned.pinned = true;
    app.push_tab(pinned, true);
    app.push_tab(
      BrowserTabState::new(tab_unpinned, "https://b.example/".to_string()),
      false,
    );

    let ctx = egui::Context::default();

    // Frame 0: render once to capture deterministic close/new-tab ids.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let close_ids = tab_strip_close_ids(&ctx);
    assert!(
      !close_ids.contains_key(&tab_pinned),
      "expected pinned tabs to have no close button in the tab strip"
    );
    let new_tab_id = tab_strip_new_tab_id(&ctx);

    let order = vec![
      tab_strip_tab_widget_id(tab_pinned),
      tab_strip_tab_widget_id(tab_unpinned),
      *close_ids
        .get(&tab_unpinned)
        .expect("expected close id for tab_unpinned"),
      new_tab_id,
    ];

    assert_tab_traversal_forward(&ctx, &mut app, &order);
    assert_tab_traversal_reverse(&ctx, &mut app, &order);
  }

  #[test]
  fn focus_traversal_tab_strip_with_pinned_and_group_chip_is_stable_expanded_and_collapsed() {
    let mut app = BrowserAppState::new();
    let tab_pinned = TabId(1);
    let tab_b = TabId(2);
    let tab_c = TabId(3);
    let tab_d = TabId(4);
    let mut pinned = BrowserTabState::new(tab_pinned, "https://pinned.example/".to_string());
    pinned.pinned = true;
    app.push_tab(pinned, true);
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_c, "https://c.example/".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_d, "https://d.example/".to_string()),
      false,
    );
    let group_id = app.create_group_with_tabs(&[tab_b, tab_c]);
    assert_ne!(group_id, TabGroupId(0));

    let ctx = egui::Context::default();

    // Expanded group.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let close_ids = tab_strip_close_ids(&ctx);
    let chip_id = ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new("test_tab_group_chip_id")))
      .expect("expected test_tab_group_chip_id to be stored");
    let new_tab_id = tab_strip_new_tab_id(&ctx);

    let expanded_order = vec![
      tab_strip_tab_widget_id(tab_pinned),
      chip_id,
      tab_strip_tab_widget_id(tab_b),
      *close_ids.get(&tab_b).expect("expected close id for tab_b"),
      tab_strip_tab_widget_id(tab_c),
      *close_ids.get(&tab_c).expect("expected close id for tab_c"),
      tab_strip_tab_widget_id(tab_d),
      *close_ids.get(&tab_d).expect("expected close id for tab_d"),
      new_tab_id,
    ];
    assert_tab_traversal_forward(&ctx, &mut app, &expanded_order);
    assert_tab_traversal_reverse(&ctx, &mut app, &expanded_order);

    // Collapsed group: member tabs disappear.
    app.toggle_group_collapsed(group_id);
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let new_tab_id = tab_strip_new_tab_id(&ctx);
    let collapsed_order = vec![
      tab_strip_tab_widget_id(tab_pinned),
      chip_id,
      tab_strip_tab_widget_id(tab_d),
      *close_ids.get(&tab_d).expect("expected close id for tab_d"),
      new_tab_id,
    ];
    assert_tab_traversal_forward(&ctx, &mut app, &collapsed_order);
    assert_tab_traversal_reverse(&ctx, &mut app, &collapsed_order);
  }

  #[test]
  fn shift_tab_focus_traversal_tab_strip_is_right_to_left() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let tab_c = TabId(3);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_c, "https://c.example/".to_string()),
      false,
    );

    let ctx = egui::Context::default();

    // Frame 0: render once to capture deterministic close/new-tab ids.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let close_ids = tab_strip_close_ids(&ctx);
    let new_tab_id = tab_strip_new_tab_id(&ctx);

    let order = vec![
      tab_strip_tab_widget_id(tab_a),
      *close_ids.get(&tab_a).expect("expected close id for tab_a"),
      tab_strip_tab_widget_id(tab_b),
      *close_ids.get(&tab_b).expect("expected close id for tab_b"),
      tab_strip_tab_widget_id(tab_c),
      *close_ids.get(&tab_c).expect("expected close id for tab_c"),
      new_tab_id,
    ];

    assert_tab_traversal_reverse(&ctx, &mut app, &order);
  }

  #[test]
  fn focus_traversal_tab_strip_with_group_chip_is_stable_expanded_and_collapsed() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let tab_c = TabId(3);
    let tab_d = TabId(4);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_c, "https://c.example/".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_d, "https://d.example/".to_string()),
      false,
    );
    let group_id = app.create_group_with_tabs(&[tab_b, tab_c]);
    assert_ne!(group_id, TabGroupId(0));

    let ctx = egui::Context::default();

    // Expanded group: the chip should appear between tab_a and tab_b.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let close_ids = tab_strip_close_ids(&ctx);
    let chip_id = ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new("test_tab_group_chip_id")))
      .expect("expected test_tab_group_chip_id to be stored");
    let new_tab_id = tab_strip_new_tab_id(&ctx);

    let expanded_order = vec![
      tab_strip_tab_widget_id(tab_a),
      *close_ids.get(&tab_a).expect("expected close id for tab_a"),
      chip_id,
      tab_strip_tab_widget_id(tab_b),
      *close_ids.get(&tab_b).expect("expected close id for tab_b"),
      tab_strip_tab_widget_id(tab_c),
      *close_ids.get(&tab_c).expect("expected close id for tab_c"),
      tab_strip_tab_widget_id(tab_d),
      *close_ids.get(&tab_d).expect("expected close id for tab_d"),
      new_tab_id,
    ];
    assert_tab_traversal_forward(&ctx, &mut app, &expanded_order);

    // Collapse the group: member tabs should disappear, leaving the chip between tab_a and tab_d.
    app.toggle_group_collapsed(group_id);
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let new_tab_id = tab_strip_new_tab_id(&ctx);
    let collapsed_order = vec![
      tab_strip_tab_widget_id(tab_a),
      *close_ids.get(&tab_a).expect("expected close id for tab_a"),
      chip_id,
      tab_strip_tab_widget_id(tab_d),
      *close_ids.get(&tab_d).expect("expected close id for tab_d"),
      new_tab_id,
    ];
    assert_tab_traversal_forward(&ctx, &mut app, &collapsed_order);
  }

  #[test]
  fn tab_strip_scrolls_focused_offscreen_tab_into_view() {
    let ctx = egui::Context::default();
    let mut app = BrowserAppState::new();

    // Create enough unpinned tabs to overflow an 800px-wide tab strip.
    const TAB_COUNT: usize = 20;
    let mut last_tab_id = TabId(0);
    for i in 0..TAB_COUNT {
      let tab_id = TabId((i + 1) as u64);
      last_tab_id = tab_id;
      app.push_tab(
        BrowserTabState::new(tab_id, format!("https://{i}.example/")),
        i == 0, // keep the first tab active so active-tab auto-scroll doesn't trigger
      );
    }

    // Frame 1: baseline render. The strip should start at scroll offset 0 and the last tab should
    // be outside the viewport.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let scroll0 = ctx
      .data(|d| d.get_temp::<f32>(egui::Id::new("test_tab_strip_unpinned_scroll_offset_x")))
      .unwrap_or(0.0);
    assert!(
      scroll0.abs() < 0.01,
      "expected initial scroll offset to be ~0, got {scroll0}"
    );

    let viewport0 = ctx
      .data(|d| {
        d.get_temp::<Option<Rect>>(egui::Id::new("test_tab_strip_unpinned_scroll_viewport_rect"))
      })
      .unwrap_or(None)
      .expect("expected unpinned scroll viewport rect to be stored");

    let (_strip_rect, tab_rects0) = load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let last_rect0 = *tab_rects0.last().expect("expected at least one tab rect");
    assert!(
      !viewport0.contains(last_rect0.center()),
      "expected last tab to start offscreen, but it was already visible. last={last_rect0:?} viewport={viewport0:?}"
    );

    // Frame 2: request focus on the last tab. The strip should auto-scroll to reveal it.
    let last_tab_widget_id = tab_strip_tab_widget_id(last_tab_id);
    ctx.memory_mut(|mem| mem.request_focus(last_tab_widget_id));
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(last_tab_widget_id)),
      "expected focus to remain on the requested tab"
    );

    let mut scroll1 = ctx
      .data(|d| d.get_temp::<f32>(egui::Id::new("test_tab_strip_unpinned_scroll_offset_x")))
      .unwrap_or(0.0);

    // Some egui versions apply `scroll_to_me` in the following frame. If we didn't scroll yet,
    // render one more frame to allow the scroll request to take effect.
    if scroll1 <= 0.1 {
      begin_frame(&ctx, Vec::new());
      let _ = render_tab_strip(&ctx, &mut app);
      let _ = ctx.end_frame();
      assert!(
        ctx.memory(|mem| mem.has_focus(last_tab_widget_id)),
        "expected focus to remain on the requested tab after scrolling"
      );
      scroll1 = ctx
        .data(|d| d.get_temp::<f32>(egui::Id::new("test_tab_strip_unpinned_scroll_offset_x")))
        .unwrap_or(0.0);
    }
    assert!(
      scroll1 > 0.1,
      "expected scroll offset to increase after focusing offscreen tab, got {scroll1}"
    );

    let viewport1 = ctx
      .data(|d| {
        d.get_temp::<Option<Rect>>(egui::Id::new("test_tab_strip_unpinned_scroll_viewport_rect"))
      })
      .unwrap_or(None)
      .expect("expected unpinned scroll viewport rect to be stored");

    let (_strip_rect, tab_rects1) = load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let last_rect1 = *tab_rects1.last().expect("expected at least one tab rect");
    assert!(
      viewport1.contains(last_rect1.center()),
      "expected last tab to be scrolled into view. last={last_rect1:?} viewport={viewport1:?}"
    );
  }

  #[test]
  fn tab_strip_scrolls_focused_offscreen_tab_close_button_into_view() {
    let ctx = egui::Context::default();
    let mut app = BrowserAppState::new();

    // Create enough unpinned tabs to overflow an 800px-wide tab strip.
    const TAB_COUNT: usize = 20;
    let mut last_tab_id = TabId(0);
    for i in 0..TAB_COUNT {
      let tab_id = TabId((i + 1) as u64);
      last_tab_id = tab_id;
      app.push_tab(
        BrowserTabState::new(tab_id, format!("https://{i}.example/")),
        i == 0, // keep the first tab active so active-tab auto-scroll doesn't trigger
      );
    }

    // Frame 1: baseline render (captures close ids/rects).
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let close_ids = tab_strip_close_ids(&ctx);
    let close_id = *close_ids
      .get(&last_tab_id)
      .expect("expected close id for last tab");

    let viewport0 = ctx
      .data(|d| {
        d.get_temp::<Option<Rect>>(egui::Id::new("test_tab_strip_unpinned_scroll_viewport_rect"))
      })
      .unwrap_or(None)
      .expect("expected unpinned scroll viewport rect to be stored");
    let close_rect0 = *tab_strip_close_rects(&ctx)
      .get(&last_tab_id)
      .expect("expected close rect for last tab");
    assert!(
      !viewport0.contains(close_rect0.center()),
      "expected last tab close button to start offscreen, but it was already visible. close={close_rect0:?} viewport={viewport0:?}"
    );

    // Frame 2: focus the close button on the last tab. The strip should auto-scroll to reveal it.
    ctx.memory_mut(|mem| mem.request_focus(close_id));
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(close_id)),
      "expected focus to remain on the requested close button"
    );

    let mut scroll1 = ctx
      .data(|d| d.get_temp::<f32>(egui::Id::new("test_tab_strip_unpinned_scroll_offset_x")))
      .unwrap_or(0.0);

    // Some egui versions apply `scroll_to_me` in the following frame. If we didn't scroll yet,
    // render one more frame to allow the scroll request to take effect.
    if scroll1 <= 0.1 {
      begin_frame(&ctx, Vec::new());
      let _ = render_tab_strip(&ctx, &mut app);
      let _ = ctx.end_frame();
      assert!(
        ctx.memory(|mem| mem.has_focus(close_id)),
        "expected focus to remain on the requested close button after scrolling"
      );
      scroll1 = ctx
        .data(|d| d.get_temp::<f32>(egui::Id::new("test_tab_strip_unpinned_scroll_offset_x")))
        .unwrap_or(0.0);
    }

    assert!(
      scroll1 > 0.1,
      "expected scroll offset to increase after focusing offscreen close button, got {scroll1}"
    );

    let viewport1 = ctx
      .data(|d| {
        d.get_temp::<Option<Rect>>(egui::Id::new("test_tab_strip_unpinned_scroll_viewport_rect"))
      })
      .unwrap_or(None)
      .expect("expected unpinned scroll viewport rect to be stored");
    let close_rect1 = *tab_strip_close_rects(&ctx)
      .get(&last_tab_id)
      .expect("expected close rect for last tab after scrolling");
    assert!(
      viewport1.contains(close_rect1.center()),
      "expected last tab close button to be scrolled into view. close={close_rect1:?} viewport={viewport1:?}"
    );
  }

  #[test]
  fn tab_a11y_label_formats_active_pinned_loading_error_warning_states() {
    let title = "Example title";
    let cases = [
      (false, false, false, false, false, "Example title"),
      (true, false, false, false, false, "Example title"),
      (false, true, false, false, false, "Example title (pinned)"),
      (true, true, false, false, false, "Example title (pinned)"),
      (
        true,
        true,
        true,
        false,
        false,
        "Example title (pinned, loading)",
      ),
      (
        false,
        true,
        false,
        true,
        false,
        "Example title (pinned, error)",
      ),
      (
        true,
        false,
        false,
        true,
        true,
        "Example title (error, warning)",
      ),
      (
        false,
        true,
        true,
        true,
        true,
        "Example title (pinned, loading, error, warning)",
      ),
      (
        true,
        true,
        true,
        true,
        true,
        "Example title (pinned, loading, error, warning)",
      ),
    ];
    for (is_active, pinned, loading, err, warn, expected) in cases {
      assert_eq!(
        crate::ui::tab_accessible_label::format_tab_accessible_label(
          title, is_active, pinned, loading, err, warn
        ),
        expected
      );
    }
  }

  #[test]
  fn drag_autoscroll_delta_x_is_zero_outside_edge_zone() {
    let rect = Rect::from_min_max(Pos2::new(100.0, 0.0), Pos2::new(200.0, 10.0));
    assert_eq!(
      drag_autoscroll_delta_x(Pos2::new(150.0, 5.0), rect, 0.016),
      0.0
    );
  }

  #[test]
  fn drag_autoscroll_delta_x_points_left_and_right() {
    let rect = Rect::from_min_max(Pos2::new(100.0, 0.0), Pos2::new(200.0, 10.0));

    let left = drag_autoscroll_delta_x(Pos2::new(100.0, 5.0), rect, 1.0);
    assert!((left + DRAG_AUTOSCROLL_MAX_SPEED_PX_PER_S).abs() < 0.01);

    let right = drag_autoscroll_delta_x(Pos2::new(200.0, 5.0), rect, 1.0);
    assert!((right - DRAG_AUTOSCROLL_MAX_SPEED_PX_PER_S).abs() < 0.01);
  }

  #[test]
  fn drag_autoscroll_delta_x_ramps_quadratically_with_proximity() {
    let rect = Rect::from_min_max(Pos2::new(100.0, 0.0), Pos2::new(200.0, 10.0));
    let zone = DRAG_AUTOSCROLL_EDGE_ZONE_PX.min(rect.width() * 0.5);
    assert!((zone - DRAG_AUTOSCROLL_EDGE_ZONE_PX).abs() < f32::EPSILON);

    // Half-way into the edge zone => t=0.5 => t^2=0.25.
    let x = rect.left() + zone * 0.5;
    let dx = drag_autoscroll_delta_x(Pos2::new(x, 5.0), rect, 1.0);
    assert!((dx + DRAG_AUTOSCROLL_MAX_SPEED_PX_PER_S * 0.25).abs() < 0.01);

    // dt should scale the delta linearly.
    let dx_half_dt = drag_autoscroll_delta_x(Pos2::new(x, 5.0), rect, 0.5);
    assert!((dx_half_dt - dx * 0.5).abs() < 0.01);
  }

  #[test]
  fn drag_autoscroll_delta_x_is_zero_when_pointer_outside_viewport_y() {
    let rect = Rect::from_min_max(Pos2::new(100.0, 0.0), Pos2::new(200.0, 10.0));
    let zone = DRAG_AUTOSCROLL_EDGE_ZONE_PX.min(rect.width() * 0.5);
    let slack = TAB_DETACH_DRAG_THRESHOLD.max(zone);
    assert_eq!(
      drag_autoscroll_delta_x(Pos2::new(rect.left(), rect.top() - slack - 1.0), rect, 1.0),
      0.0
    );
    assert_eq!(
      drag_autoscroll_delta_x(
        Pos2::new(rect.left(), rect.bottom() + slack + 1.0),
        rect,
        1.0
      ),
      0.0
    );
  }

  #[test]
  fn drag_autoscroll_delta_x_is_zero_when_pointer_far_outside_viewport_x() {
    let rect = Rect::from_min_max(Pos2::new(100.0, 0.0), Pos2::new(200.0, 10.0));
    let zone = DRAG_AUTOSCROLL_EDGE_ZONE_PX.min(rect.width() * 0.5);
    let slack = TAB_DETACH_DRAG_THRESHOLD.max(zone);
    assert_eq!(
      drag_autoscroll_delta_x(
        Pos2::new(rect.left() - slack - 1.0, rect.center().y),
        rect,
        1.0
      ),
      0.0
    );
    assert_eq!(
      drag_autoscroll_delta_x(
        Pos2::new(rect.right() + slack + 1.0, rect.center().y),
        rect,
        1.0
      ),
      0.0
    );
  }

  #[test]
  fn scroll_clamp_snaps_when_egui_animation_time_is_zero() {
    let ctx = egui::Context::default();
    let mut style = egui::Style::default();
    style.animation_time = 0.0;
    ctx.set_style(style);

    let motion = UiMotion::new(true);
    let clamp_anim_id = egui::Id::new("test_scroll_clamp_anim");

    let desired_scroll_offset_x = 200.0;
    let max_scroll_x = 100.0;
    let now = 1.0_f64;

    let (clamped, end_spacer_x, clamping) = tab_strip_scroll_clamp(
      &ctx,
      motion,
      clamp_anim_id,
      desired_scroll_offset_x,
      max_scroll_x,
      now,
    );

    assert!(
      clamping,
      "expected clamp to trigger when offset exceeds max"
    );
    assert!(
      (clamped - max_scroll_x).abs() < 1e-6,
      "expected clamp to snap immediately when animation_time=0"
    );
    assert!(
      end_spacer_x.abs() < 1e-6,
      "expected no end spacer when snapping immediately"
    );

    let anim = ctx
      .data(|d| d.get_temp::<TabStripScrollClampAnim>(clamp_anim_id))
      .unwrap_or_default();
    assert!(
      !anim.active,
      "expected no active animation when animation_time=0"
    );
  }

  #[test]
  fn scroll_clamp_snaps_when_motion_disabled() {
    let ctx = egui::Context::default();
    let motion = UiMotion::new(false);
    let clamp_anim_id = egui::Id::new("test_scroll_clamp_anim_motion_off");

    let desired_scroll_offset_x = 200.0;
    let max_scroll_x = 100.0;
    let now = 1.0_f64;

    let (clamped, end_spacer_x, clamping) = tab_strip_scroll_clamp(
      &ctx,
      motion,
      clamp_anim_id,
      desired_scroll_offset_x,
      max_scroll_x,
      now,
    );

    assert!(
      clamping,
      "expected clamp to trigger when offset exceeds max"
    );
    assert!(
      (clamped - max_scroll_x).abs() < 1e-6,
      "expected clamp to snap immediately when motion is disabled"
    );
    assert!(
      end_spacer_x.abs() < 1e-6,
      "expected no end spacer when snapping immediately"
    );

    let anim = ctx
      .data(|d| d.get_temp::<TabStripScrollClampAnim>(clamp_anim_id))
      .unwrap_or_default();
    assert!(
      !anim.active,
      "expected no active animation when motion is disabled"
    );
  }

  #[test]
  fn scroll_clamp_restarts_when_user_scrolls_mid_clamp() {
    let ctx = egui::Context::default();
    let mut style = egui::Style::default();
    style.animation_time = 1.0;
    ctx.set_style(style);

    let motion = UiMotion::new(true);
    let clamp_anim_id = egui::Id::new("test_scroll_clamp_anim_user_scroll");

    let max_scroll_x = 100.0;
    let desired_scroll_offset_x = 200.0;

    // Frame 1: start clamping at an out-of-range scroll offset.
    let (clamped0, _end_spacer0, clamping0) = tab_strip_scroll_clamp(
      &ctx,
      motion,
      clamp_anim_id,
      desired_scroll_offset_x,
      max_scroll_x,
      0.0,
    );
    assert!(clamping0);
    assert!((clamped0 - desired_scroll_offset_x).abs() < 1e-6);

    // Frame 2: simulate the user scrolling while the clamp animation is active. The clamp should
    // restart from the user's new position, rather than continuing along the old animation track.
    let user_offset_x = 180.0;
    let (clamped1, _end_spacer1, clamping1) = tab_strip_scroll_clamp(
      &ctx,
      motion,
      clamp_anim_id,
      user_offset_x,
      max_scroll_x,
      0.05,
    );
    assert!(clamping1);
    assert!(
      (clamped1 - user_offset_x).abs() < 1e-6,
      "expected clamp to restart from user scroll offset"
    );

    // Frame 3: with no further user scrolling, the clamp should now progress toward the max scroll
    // based on the restarted animation.
    let (clamped2, _end_spacer2, clamping2) = tab_strip_scroll_clamp(
      &ctx,
      motion,
      clamp_anim_id,
      user_offset_x,
      max_scroll_x,
      0.10,
    );
    assert!(clamping2);
    assert!(clamped2 < user_offset_x - 0.01);
    assert!(clamped2 > max_scroll_x - 1e-6);
  }

  #[test]
  fn sanitize_tab_width_rejects_non_finite() {
    assert_eq!(sanitize_tab_width(f32::NAN), 0.0);
    assert_eq!(sanitize_tab_width(f32::INFINITY), 0.0);
    assert_eq!(sanitize_tab_width(f32::NEG_INFINITY), 0.0);
  }

  #[test]
  fn sanitize_tab_width_clamps_negative_to_zero() {
    assert_eq!(sanitize_tab_width(-1.0), 0.0);
    assert_eq!(sanitize_tab_width(-0.0), 0.0);
  }

  #[test]
  fn sizing_without_overflow_prefers_max_width() {
    let sizing = compute_tab_strip_sizing(1200.0, 3);
    assert!(!sizing.overflow);
    assert!((sizing.tab_width - TAB_MAX_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_overflows_when_min_width_cannot_fit() {
    let sizing = compute_tab_strip_sizing(400.0, 10);
    assert!(sizing.overflow);
    assert!((sizing.tab_width - TAB_MIN_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_is_non_overflow_when_exact_min_width_fits() {
    let tabs: usize = 6;
    let required = (tabs as f32) * TAB_MIN_WIDTH + (tabs.saturating_sub(1) as f32) * TAB_GAP;
    let sizing = compute_tab_strip_sizing(required, tabs);
    assert!(!sizing.overflow);
    assert!((sizing.tab_width - TAB_MIN_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_with_zero_extras_matches_tabs_only() {
    let tabs: usize = 5;
    let available = 777.0;
    let sizing_tabs = compute_tab_strip_sizing(available, tabs);
    let sizing_extras =
      compute_tab_strip_sizing_with_fixed_width(available, tabs, 0.0, tabs.saturating_sub(1));
    assert!((sizing_tabs.tab_width - sizing_extras.tab_width).abs() < f32::EPSILON);
    assert_eq!(sizing_tabs.overflow, sizing_extras.overflow);
    assert!(
      (sizing_tabs.total_content_width - sizing_extras.total_content_width).abs() < f32::EPSILON
    );
  }

  #[test]
  fn sizing_accounts_for_extra_item_width() {
    let available = 600.0;
    let tabs: usize = 3;
    let chip_width = 120.0;

    let sizing_no_chips = compute_tab_strip_sizing(available, tabs);
    assert!((sizing_no_chips.tab_width - 196.0).abs() < 0.01);

    // 3 tabs + 1 chip => 4 items => 3 gaps.
    let sizing_with_chip =
      compute_tab_strip_sizing_with_fixed_width(available, tabs, chip_width, tabs);
    assert!((sizing_with_chip.tab_width - 154.0).abs() < 0.01);
    assert!(!sizing_with_chip.overflow);
  }

  #[test]
  fn sizing_overflow_flips_when_extras_added() {
    let tabs: usize = 4;
    let available = (tabs as f32) * TAB_MIN_WIDTH + (tabs.saturating_sub(1) as f32) * TAB_GAP;
    let sizing_no_chips = compute_tab_strip_sizing(available, tabs);
    assert!(!sizing_no_chips.overflow);

    // One extra fixed-width chip (plus the extra gap it introduces) should force overflow even
    // though the same viewport fits tabs at `TAB_MIN_WIDTH` without it.
    // 4 tabs + 1 chip => 5 items => 4 gaps.
    let sizing_with_chip = compute_tab_strip_sizing_with_fixed_width(available, tabs, 100.0, tabs);
    assert!(sizing_with_chip.overflow);
    assert!((sizing_with_chip.tab_width - TAB_MIN_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_overflow_accounts_for_chip_gaps_even_with_zero_chip_width() {
    // A zero-width "extra item" is unrealistic for a real tab group chip, but it makes the test
    // specifically validate that *gaps introduced by extra items* participate in overflow.
    let tabs: usize = 1;
    let available = TAB_MIN_WIDTH;
    let sizing_no_chip = compute_tab_strip_sizing(available, tabs);
    assert!(!sizing_no_chip.overflow);

    // 1 tab + 1 extra item => 2 items => 1 gap.
    let sizing_with_chip_gap = compute_tab_strip_sizing_with_fixed_width(available, tabs, 0.0, 1);
    assert!(sizing_with_chip_gap.overflow);
    assert!((sizing_with_chip_gap.tab_width - TAB_MIN_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_clamps_non_finite_and_negative_inputs() {
    let tabs: usize = 3;

    // Non-finite available width is treated as 0 (matches the original sizing helper semantics).
    let sizing_nan = compute_tab_strip_sizing_with_fixed_width(f32::NAN, tabs, 0.0, tabs - 1);
    let sizing_zero = compute_tab_strip_sizing_with_fixed_width(0.0, tabs, 0.0, tabs - 1);
    assert_eq!(sizing_nan, sizing_zero);

    // Non-finite/negative fixed widths are treated as 0.
    let available = 500.0;
    let sizing_inf_fixed =
      compute_tab_strip_sizing_with_fixed_width(available, tabs, f32::INFINITY, tabs - 1);
    let sizing_neg_fixed =
      compute_tab_strip_sizing_with_fixed_width(available, tabs, -10.0, tabs - 1);
    let sizing_fixed_zero =
      compute_tab_strip_sizing_with_fixed_width(available, tabs, 0.0, tabs - 1);
    assert_eq!(sizing_inf_fixed, sizing_fixed_zero);
    assert_eq!(sizing_neg_fixed, sizing_fixed_zero);
  }

  #[test]
  fn sizing_scaled_tabs_matches_fixed_width_when_no_scaling() {
    let available = 600.0;
    let tabs: usize = 3;
    let chip_width = 120.0;
    // 3 tabs + 1 chip => 4 items => 3 gaps.
    let gap_count = tabs;

    let sizing_fixed =
      compute_tab_strip_sizing_with_fixed_width(available, tabs, chip_width, gap_count);
    let sizing_scaled = compute_tab_strip_sizing_with_scaled_tabs(
      available,
      tabs as f32,
      chip_width,
      TAB_GAP * (gap_count as f32),
    );
    assert_eq!(sizing_fixed, sizing_scaled);
  }

  #[test]
  fn sizing_scaled_tabs_supports_fractional_tabs_and_gaps() {
    // Model one partially collapsed group tab (0.5) adjacent to a normal tab (1.0). With a single
    // gap between them scaled by `min(0.5, 1.0) = 0.5`, the total gap width is `TAB_GAP * 0.5`.
    let available = 200.0;
    let tab_units = 1.5;
    let total_gap_width = TAB_GAP * 0.5;

    let sizing =
      compute_tab_strip_sizing_with_scaled_tabs(available, tab_units, 0.0, total_gap_width);
    assert!(sizing.overflow);
    assert!((sizing.tab_width - TAB_MIN_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_with_group_chips_can_overflow_even_when_few_tabs() {
    // 2 tabs + 1 chip => 3 items => 2 gaps.
    let sizing = compute_tab_strip_sizing_with_fixed_width(400.0, 2, 160.0, 2);
    assert!(sizing.overflow);
    assert!((sizing.tab_width - TAB_MIN_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_with_only_group_chips_reports_overflow() {
    // 0 tabs + 2 chips => 2 items => 1 gap.
    let sizing = compute_tab_strip_sizing_with_fixed_width(200.0, 0, 240.0, 1);
    assert!(sizing.overflow);
    assert!((sizing.tab_width - 0.0).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_scaled_tabs_matches_formula_without_clamp() {
    // Choose parameters that keep the computed width comfortably within [TAB_MIN_WIDTH, TAB_MAX_WIDTH]
    // so we can assert the exact algebraic result.
    let available = 600.0;
    let tab_units = 2.5;
    let extra = 120.0;
    let gaps = TAB_GAP * 3.0;

    let sizing = compute_tab_strip_sizing_with_scaled_tabs(available, tab_units, extra, gaps);
    let expected_tab_width = (available - extra - gaps) / tab_units;
    assert!((sizing.tab_width - expected_tab_width).abs() < 0.01);
    assert!(!sizing.overflow);
    assert!((sizing.total_content_width - available).abs() < 0.01);
  }

  #[test]
  fn sizing_scaled_tabs_clamps_and_overflows_like_min_width() {
    // Force a case where the ideal width would go below TAB_MIN_WIDTH and the strip should overflow.
    let available = 400.0;
    let tab_units = 2.0;
    let extra = 160.0;
    let gaps = TAB_GAP * 2.0;

    let sizing = compute_tab_strip_sizing_with_scaled_tabs(available, tab_units, extra, gaps);
    assert!(sizing.overflow);
    assert!((sizing.tab_width - TAB_MIN_WIDTH).abs() < f32::EPSILON);
  }

  #[test]
  fn pinned_viewport_never_exceeds_total() {
    for total in [0.0_f32, 10.0, 200.0, 400.0] {
      let (pinned, unpinned) = compute_pinned_viewport_width(total, 10_000.0, true);
      assert!(
        pinned <= total + 1e-3,
        "pinned viewport ({pinned}) should not exceed total ({total})"
      );
      assert!(
        unpinned >= -1e-3,
        "unpinned viewport should never be negative"
      );
      assert!(
        pinned + unpinned <= total + 1e-3,
        "sum of viewports should not exceed total width"
      );
    }
  }

  #[test]
  fn pinned_viewport_reserves_min_unpinned_width_when_possible() {
    // Pick a width where the unpinned minimum is satisfiable, and pinned is constrained primarily
    // by the unpinned-minimum rule (not by pinned_content_width).
    let total = MIN_UNPINNED_VIEWPORT + PINNED_TAB_WIDTH + TAB_GAP + 1.0;
    let (pinned, unpinned) = compute_pinned_viewport_width(total, 10_000.0, true);
    assert!(
      unpinned + 1e-3 >= MIN_UNPINNED_VIEWPORT,
      "expected unpinned viewport to keep minimum width"
    );
    assert!(
      (total - pinned - unpinned) >= -1e-3,
      "expected gap to be non-negative"
    );
  }

  #[test]
  fn pinned_viewport_uses_content_width_when_under_caps() {
    let total = 800.0;
    let pinned_content = PINNED_TAB_WIDTH * 2.0 + TAB_GAP;
    let (pinned, unpinned) = compute_pinned_viewport_width(total, pinned_content, true);
    assert!(
      (pinned - pinned_content).abs() < 0.01,
      "expected pinned viewport to match content width when it fits"
    );
    let gap = total - pinned - unpinned;
    assert!(
      (gap - TAB_GAP).abs() < 0.01,
      "expected default inter-segment gap"
    );
  }

  #[test]
  fn pinned_viewport_is_zero_when_no_pinned_content() {
    let total = 500.0;
    let (pinned, unpinned) = compute_pinned_viewport_width(total, 0.0, true);
    assert!((pinned - 0.0).abs() < f32::EPSILON);
    assert!((unpinned - total).abs() < f32::EPSILON);
  }

  #[test]
  fn pinned_viewport_takes_full_width_when_only_pinned_tabs_exist() {
    let total = 500.0;
    let (pinned, unpinned) = compute_pinned_viewport_width(total, 10_000.0, false);
    assert!((pinned - total).abs() < f32::EPSILON);
    assert!((unpinned - 0.0).abs() < f32::EPSILON);
  }

  #[test]
  fn pinned_viewport_drops_gap_when_strip_is_too_narrow() {
    let total = PINNED_TAB_WIDTH + 2.0;
    let (pinned, unpinned) = compute_pinned_viewport_width(total, 10_000.0, true);
    assert!(
      unpinned > 0.0,
      "expected some width reserved for unpinned viewport"
    );
    let gap = total - pinned - unpinned;
    assert!(
      gap.abs() < 0.01,
      "expected gap to be dropped under narrow widths"
    );
  }

  #[test]
  fn pinned_viewport_sanitizes_invalid_inputs() {
    let total = 250.0;

    // Non-finite / negative pinned content should behave like "no pinned tabs".
    for pinned_content in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -10.0] {
      let (pinned, unpinned) = compute_pinned_viewport_width(total, pinned_content, true);
      assert!(pinned.is_finite() && unpinned.is_finite());
      assert!((pinned - 0.0).abs() < f32::EPSILON);
      assert!((unpinned - total).abs() < f32::EPSILON);
    }

    // Non-finite / negative total widths should clamp to zero so we never return NaNs.
    for total_width in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -10.0] {
      let (pinned, unpinned) = compute_pinned_viewport_width(total_width, PINNED_TAB_WIDTH, true);
      assert!(pinned.is_finite() && unpinned.is_finite());
      assert!((pinned - 0.0).abs() < f32::EPSILON);
      assert!((unpinned - 0.0).abs() < f32::EPSILON);
    }
  }

  #[test]
  fn group_chip_a11y_label_uses_expand_collapse_semantics() {
    assert_eq!(
      group_chip_a11y_label("Reading list", true),
      "Expand tab group: Reading list"
    );
    assert_eq!(
      group_chip_a11y_label("Reading list", false),
      "Collapse tab group: Reading list"
    );
  }

  #[test]
  fn group_chip_width_is_based_on_title_only_and_stable_across_collapsed_state() {
    let ctx = egui::Context::default();
    begin_frame(&ctx, Vec::new());

    let mut found_case = false;
    egui::CentralPanel::default().show(&ctx, |ui| {
      for len in 1..128 {
        let title = "W".repeat(len);
        let group_id = TabGroupId(1);
        let group_collapsed = TabGroupState {
          id: group_id,
          title: title.clone(),
          color: TabGroupColor::Blue,
          collapsed: true,
          tab_group_chip_a11y_label_cache:
            crate::ui::tab_accessible_label::TitlePrefixedLabelCache::default(),
        };
        let mut group_expanded = group_collapsed.clone();
        group_expanded.collapsed = false;

        let title_collapsed = group_chip_title(&group_collapsed);
        let title_expanded = group_chip_title(&group_expanded);
        assert_eq!(title_collapsed, title.as_str());
        assert_eq!(title_expanded, title.as_str());

        let width_collapsed = group_chip_width(ui, title_collapsed);
        let width_expanded = group_chip_width(ui, title_expanded);
        // The chip's collapse icon is painted at a fixed size, so the width should not depend on
        // collapsed/expanded state.
        assert!((width_collapsed - width_expanded).abs() < 0.01);

        // Ensure we picked a title where the clamp isn't masking the effect.
        if width_collapsed <= GROUP_CHIP_MIN_WIDTH + 0.5
          || width_collapsed >= GROUP_CHIP_MAX_WIDTH - 0.5
        {
          continue;
        }

        // If the width calculation accidentally included the old arrow glyph + space, we'd
        // over-estimate the chip width. This would shrink tab widths / trigger overflow earlier.
        let arrow_label = format!("▸ {title}");
        let arrow_width = group_chip_width(ui, &arrow_label);
        if arrow_width > width_collapsed + 0.01 {
          found_case = true;
          break;
        }
      }

      assert!(
        found_case,
        "expected to find a title where an arrow-prefixed label measures wider than title-only"
      );
    });

    let _ = ctx.end_frame();
  }

  #[test]
  fn insertion_index_beginning_middle_end() {
    let rects = vec![
      (
        TabId(1),
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(10.0, 10.0)),
      ),
      (
        TabId(2),
        Rect::from_min_max(Pos2::new(20.0, 0.0), Pos2::new(30.0, 10.0)),
      ),
      (
        TabId(3),
        Rect::from_min_max(Pos2::new(40.0, 0.0), Pos2::new(50.0, 10.0)),
      ),
    ];

    let dragged = TabId(2);
    assert_eq!(compute_tab_insertion_index(-5.0, &rects, dragged), 0);
    assert_eq!(compute_tab_insertion_index(25.0, &rects, dragged), 1);
    assert_eq!(compute_tab_insertion_index(100.0, &rects, dragged), 2);
  }

  #[test]
  fn insertion_index_ignores_dragged_tab() {
    let rects = vec![
      (
        TabId(1),
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(10.0, 10.0)),
      ),
      (
        TabId(2),
        Rect::from_min_max(Pos2::new(20.0, 0.0), Pos2::new(30.0, 10.0)),
      ),
      (
        TabId(3),
        Rect::from_min_max(Pos2::new(40.0, 0.0), Pos2::new(50.0, 10.0)),
      ),
    ];

    let dragged = TabId(2);
    // Pointer over the dragged tab's center should still compute an insertion index based on the
    // other tabs only.
    assert_eq!(compute_tab_insertion_index(25.0, &rects, dragged), 1);
  }

  #[test]
  fn insertion_index_center_boundary_is_stable() {
    let rects = vec![
      (
        TabId(1),
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(10.0, 10.0)),
      ),
      (
        TabId(2),
        Rect::from_min_max(Pos2::new(20.0, 0.0), Pos2::new(30.0, 10.0)),
      ),
      (
        TabId(3),
        Rect::from_min_max(Pos2::new(40.0, 0.0), Pos2::new(50.0, 10.0)),
      ),
    ];

    let dragged = TabId(2);
    let center = rects[0].1.center().x;
    // Exact equality should deterministically pick the "after" side.
    assert_eq!(compute_tab_insertion_index(center, &rects, dragged), 1);
  }

  #[test]
  fn insertion_index_handles_non_finite_pointer_x() {
    let rects = vec![
      (
        TabId(1),
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(10.0, 10.0)),
      ),
      (
        TabId(2),
        Rect::from_min_max(Pos2::new(20.0, 0.0), Pos2::new(30.0, 10.0)),
      ),
      (
        TabId(3),
        Rect::from_min_max(Pos2::new(40.0, 0.0), Pos2::new(50.0, 10.0)),
      ),
    ];

    let dragged = TabId(2);
    // NaN/NEG_INFINITY are treated as "before the strip".
    assert_eq!(compute_tab_insertion_index(f32::NAN, &rects, dragged), 0);
    assert_eq!(
      compute_tab_insertion_index(f32::NEG_INFINITY, &rects, dragged),
      0
    );
    // +INF should behave like a pointer far to the right (after all other tabs).
    assert_eq!(
      compute_tab_insertion_index(f32::INFINITY, &rects, dragged),
      2
    );
  }

  #[test]
  fn tab_close_button_is_keyboard_activatable() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );

    let ctx = egui::Context::default();

    // Frame 0: render once to capture deterministic close ids.
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let _ = ctx.end_frame();

    let close_ids = ctx
      .data(|d| d.get_temp::<Vec<(TabId, egui::Id)>>(egui::Id::new("test_tab_strip_close_ids")))
      .expect("expected test_tab_strip_close_ids");
    let close_id = close_ids
      .iter()
      .find_map(|(tab_id, close_id)| (*tab_id == tab_a).then_some(*close_id))
      .expect("expected close id for active tab");

    for key in [egui::Key::Enter, egui::Key::Space] {
      ctx.memory_mut(|mem| mem.request_focus(close_id));
      begin_frame(&ctx, vec![key_press(key)]);
      let actions = render_tab_strip(&ctx, &mut app);
      let _ = ctx.end_frame();

      assert!(
        actions
          .iter()
          .any(|action| matches!(action, ChromeAction::CloseTab(id) if *id == tab_a)),
        "expected ChromeAction::CloseTab({tab_a:?}) for key={key:?}, got {actions:?}"
      );
    }
  }

  #[test]
  fn tab_strip_emits_accesskit_name_for_new_tab_button() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.time = Some(0.0);
    raw.focused = true;
    ctx.begin_frame(raw);

    egui::CentralPanel::default().show(&ctx, |ui| {
      let motion = UiMotion::from_ctx(ui.ctx());
      let focus_ring = super::super::chrome_focus_ring_style(ui.ctx(), &app);
      let mut favicon = |_tab_id: TabId| None;
      let _actions = tab_strip_ui(ui, &mut app, &mut favicon, motion, focus_ring);
    });

    let output = ctx.end_frame();
    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    assert!(
      names.iter().any(|n| n == BrowserIcon::NewTab.a11y_label()),
      "expected New tab button name in AccessKit output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn tab_strip_tabs_have_accesskit_tab_role_and_selected_state() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );

    // Ensure we cover both `pinned_tab_ui` and `tab_ui`.
    assert!(app.pin_tab(tab_a));

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let find_tab_node = |output: &egui::FullOutput, name_prefix: &str| {
      let update = output
        .platform_output
        .accesskit_update
        .as_ref()
        .expect("egui did not emit an AccessKit update (did you forget ctx.enable_accesskit()?)");
      let mut matches = update
        .nodes
        .iter()
        .filter(|(_id, node)| node.name().unwrap_or("").trim().starts_with(name_prefix));
      let (id, node) = matches
        .next()
        .unwrap_or_else(|| panic!("expected AccessKit node with name prefix {name_prefix:?}"));
      assert!(
        matches.next().is_none(),
        "expected a single AccessKit node with name prefix {name_prefix:?}"
      );
      (*id, node)
    };

    // Frame 1: tab A is active (selected).
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let output0 = ctx.end_frame();

    let (tab_a_id_0, tab_a_node_0) = find_tab_node(&output0, "https://a.example/");
    let (tab_b_id_0, tab_b_node_0) = find_tab_node(&output0, "https://b.example/");

    assert_eq!(
      tab_a_node_0.role(),
      accesskit::Role::Tab,
      "expected active tab to expose AccessKit Role::Tab"
    );
    assert_eq!(
      tab_a_node_0.is_selected(),
      Some(true),
      "expected active tab to expose selected=true"
    );
    assert_eq!(
      tab_b_node_0.role(),
      accesskit::Role::Tab,
      "expected inactive tab to expose AccessKit Role::Tab"
    );
    assert_eq!(
      tab_b_node_0.is_selected(),
      Some(false),
      "expected inactive tab to expose selected=false"
    );

    // Frame 2: switch active tab and ensure selection follows without changing the tab node ids.
    assert!(app.set_active_tab(tab_b));
    begin_frame(&ctx, Vec::new());
    let _ = render_tab_strip(&ctx, &mut app);
    let output1 = ctx.end_frame();

    let (tab_a_id_1, tab_a_node_1) = find_tab_node(&output1, "https://a.example/");
    let (tab_b_id_1, tab_b_node_1) = find_tab_node(&output1, "https://b.example/");

    assert_eq!(
      tab_a_id_0, tab_a_id_1,
      "expected tab A to keep a stable AccessKit id across selection changes"
    );
    assert_eq!(
      tab_b_id_0, tab_b_id_1,
      "expected tab B to keep a stable AccessKit id across selection changes"
    );
    assert_eq!(tab_a_node_1.is_selected(), Some(false));
    assert_eq!(tab_b_node_1.is_selected(), Some(true));
  }
}
