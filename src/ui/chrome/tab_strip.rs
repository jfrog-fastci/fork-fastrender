use crate::ui::browser_app::{
  BrowserAppState, BrowserTabState, ChromeState, OpenTabContextMenuState, TabGroupColor, TabGroupId,
};
use crate::ui::icons::paint_icon_in_rect;
use crate::ui::messages::TabId;
use crate::ui::motion::UiMotion;
use crate::ui::{icon_button, BrowserIcon};
use egui::{Align2, Color32, FontId, Pos2, Rect, Response, Sense, Stroke, Vec2};
use std::collections::HashMap;

use super::ChromeAction;
use super::FocusRingStyle;

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

// Tab-strip drag auto-scroll parameters (when the unpinned segment overflows).
const DRAG_AUTOSCROLL_EDGE_ZONE_PX: f32 = 36.0;
const DRAG_AUTOSCROLL_MAX_SPEED_PX_PER_S: f32 = 1200.0;

fn drag_autoscroll_delta_x(pointer_pos: Pos2, viewport_rect: Rect, dt: f32) -> f32 {
  if dt <= 0.0 || !dt.is_finite() || viewport_rect.width() <= 0.0 {
    return 0.0;
  }

  // Don't let the activation zone exceed half the viewport width.
  let zone = DRAG_AUTOSCROLL_EDGE_ZONE_PX.min(viewport_rect.width() * 0.5);
  if zone <= 0.0 {
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

fn tab_a11y_label(title: &str, loading: bool, has_error: bool, has_warning: bool) -> String {
  let mut parts: Vec<&'static str> = Vec::new();
  if loading {
    parts.push("loading");
  }
  if has_error {
    parts.push("error");
  }
  if has_warning {
    parts.push("warning");
  }
  if parts.is_empty() {
    title.to_string()
  } else {
    format!("{title} ({})", parts.join(", "))
  }
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
  let mut colors = Vec::new();
  if error_t > 0.0 {
    colors.push(with_alpha(visuals.error_fg_color, error_t));
  }
  if warning_t > 0.0 {
    colors.push(with_alpha(visuals.warn_fg_color, warning_t));
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
  for color in colors {
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

fn group_color_egui(color: TabGroupColor) -> Color32 {
  let (r, g, b) = color.rgb();
  Color32::from_rgb(r, g, b)
}

fn group_color_fill(color: TabGroupColor) -> Color32 {
  let (r, g, b) = color.rgb();
  Color32::from_rgba_unmultiplied(r, g, b, 48)
}

#[cfg(feature = "browser_ui")]
fn drag_offset_id() -> egui::Id {
  egui::Id::new("tab_strip_drag_offset")
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
  let stroke_color = group_color.unwrap_or(visuals.widgets.active.bg_stroke.color);
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
  let tab_id = ui.make_persistent_id(("tab_strip_tab", tab.id));
  let title = tab.display_title();
  let (err, warn) = tab_status_messages(tab);
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
    placeholder_favicon(ui.painter(), icon_rect, &visuals);
    let glyph = title
      .trim()
      .chars()
      .next()
      .map(|ch| ch.to_ascii_uppercase().to_string())
      .unwrap_or_else(|| "?".to_string());
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
    let time = if motion.enabled {
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
    err.is_some(),
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    warn.is_some(),
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
  let tab_id = ui.make_persistent_id(("tab_strip_tab", tab.id));
  let title = tab.display_title();
  let (err, warn) = tab_status_messages(tab);
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
    placeholder_favicon(ui.painter(), icon_rect, &visuals);
    let glyph = title
      .trim()
      .chars()
      .next()
      .map(|ch| ch.to_ascii_uppercase().to_string())
      .unwrap_or_else(|| "?".to_string());
    ui.painter().text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(14.0),
      visuals.text_color(),
    );
  }

  if tab.loading {
    let time = if motion.enabled {
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
    err.is_some(),
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    warn.is_some(),
    motion.durations.progress_fade,
  );
  paint_tab_status_badges(ui.painter(), icon_rect, &visuals, err_t, warn_t);
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

  let gaps = TAB_GAP * (gap_count as f32);

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

pub(super) fn compute_tab_strip_sizing(available_width: f32, tab_count: usize) -> TabStripSizing {
  compute_tab_strip_sizing_with_fixed_width(
    available_width,
    tab_count,
    0.0,
    tab_count.saturating_sub(1),
  )
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
  for i in 0..n {
    let t = i as f32 / n as f32;
    // Newest segment is brightest.
    let alpha = (255.0 * (1.0 - t).powf(2.0)).round().clamp(0.0, 255.0) as u8;
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

fn placeholder_favicon(painter: &egui::Painter, rect: Rect, visuals: &egui::Visuals) {
  let fill = visuals.widgets.inactive.bg_fill;
  let stroke = visuals.widgets.inactive.bg_stroke;
  // Keep favicon placeholders subtly rounded without looking fully pill-shaped.
  let rounding = egui::Rounding::same((visuals.widgets.inactive.rounding.nw * 0.5).clamp(2.0, 4.0));
  painter.rect_filled(rect, rounding, fill);
  painter.rect_stroke(rect, rounding, stroke);
}

fn group_chip_width(ui: &egui::Ui, label: &str) -> f32 {
  let font_id = egui::TextStyle::Button.resolve(ui.style());
  let galley =
    ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id, ui.visuals().text_color()));
  // Reserve fixed space for the collapse/expand affordance icon so the chip width remains stable
  // across collapsed/expanded states.
  (galley.size().x + GROUP_CHIP_PADDING_X * 2.0 + GROUP_CHIP_ICON_SIZE + GROUP_CHIP_ICON_GAP)
    .clamp(GROUP_CHIP_MIN_WIDTH, GROUP_CHIP_MAX_WIDTH)
}

fn group_chip_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  app: &mut BrowserAppState,
  group_id: TabGroupId,
  ops: &mut Vec<TabStripOp>,
  focus_ring: FocusRingStyle,
) {
  let Some(group) = app.tab_groups.get(&group_id) else {
    return;
  };

  let color = group.color;
  let collapsed = group.collapsed;
  let title = if group.title.trim().is_empty() {
    "Group".to_string()
  } else {
    group.title.clone()
  };

  let id = ui.make_persistent_id(("tab_group_chip", group_id.0));
  let width = group_chip_width(ui, &title);
  let (_, chip_rect) = ui.allocate_space(Vec2::new(width, TAB_HEIGHT));
  let mut response = ui.interact(chip_rect, id, Sense::click());
  if response.hovered() {
    response = response.on_hover_text(title.as_str());
  }
  response.widget_info({
    let label = format!("Tab group: {title}");
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
  });

  let visuals = ui.style().visuals.clone();

  // Micro-interaction: fade hover highlight in/out (keeping group color identity).
  let hover_t = motion.animate_bool(
    ui.ctx(),
    id.with("hover"),
    response.hovered(),
    motion.durations.hover_fade,
  );

  let (r, g, b) = color.rgb();
  let fill_base = Color32::from_rgba_unmultiplied(r, g, b, 48);
  let fill_hover = Color32::from_rgba_unmultiplied(r, g, b, 72);
  let fill_active = Color32::from_rgba_unmultiplied(r, g, b, 92);
  let mut fill = lerp_color(fill_base, fill_hover, hover_t);
  if response.is_pointer_button_down_on() {
    fill = fill_active;
  }

  let stroke_base = with_alpha(group_color_egui(color), 0.85);
  let stroke_hover = group_color_egui(color);
  let stroke_color = lerp_color(stroke_base, stroke_hover, hover_t);
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
    motion.durations.tab_underline,
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
    let label = egui::Label::new(egui::RichText::new(title.clone()))
      .truncate(true)
      .wrap(false);
    let _ = ui.put(title_rect, label);
  }

  if response.clicked() {
    ops.push(TabStripOp::ToggleGroupCollapsed(group_id));
  }

  response = response.context_menu(|ui| {
    ui.label("Rename group");
    let mut new_title = app
      .tab_groups
      .get(&group_id)
      .map(|g| g.title.clone())
      .unwrap_or_default();
    let resp = ui.text_edit_singleline(&mut new_title);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Group name"));
    if resp.changed() {
      app.set_group_title(group_id, new_title);
    }

    ui.separator();

    let change_color_menu = ui.menu_button("Change color", |ui| {
      for color in TabGroupColor::ALL {
        let button = egui::Button::new(color.as_str())
          .fill(group_color_fill(color))
          .stroke(Stroke::new(1.0, group_color_egui(color)));
        let resp = ui.add(button);
        resp.widget_info({
          let label = format!("Set group color: {}", color.as_str());
          move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
        });
        if resp.clicked() {
          ops.push(TabStripOp::SetGroupColor(group_id, color));
          ui.close_menu();
        }
      }
    });
    change_color_menu
      .response
      .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Change group color"));

    ui.separator();

    let ungroup = ui.button("Ungroup");
    ungroup.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Ungroup"));
    if ungroup.clicked() {
      ops.push(TabStripOp::Ungroup(group_id));
      ui.close_menu();
    }

    let label = if collapsed {
      "Expand group"
    } else {
      "Collapse group"
    };
    let collapse_toggle = ui.button(label);
    collapse_toggle.widget_info({
      let label = label.to_string();
      move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
    });
    if collapse_toggle.clicked() {
      ops.push(TabStripOp::ToggleGroupCollapsed(group_id));
      ui.close_menu();
    }
  });

  super::paint_focus_ring(ui, &response, focus_ring);
}

fn tab_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  tab: &BrowserTabState,
  is_active: bool,
  interactive: bool,
  can_close_tabs: bool,
  tab_width: f32,
  favicon_tex: Option<egui::TextureId>,
  chrome: &mut ChromeState,
  focus_ring: FocusRingStyle,
  group_color: Option<Color32>,
) -> (Rect, Response, Option<ChromeAction>) {
  let (_, tab_rect) = ui.allocate_space(Vec2::new(tab_width, TAB_HEIGHT));
  let tab_id = ui.make_persistent_id(("tab_strip_tab", tab.id));
  let title = tab.display_title();
  let (err, warn) = tab_status_messages(tab);
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
  if hovered {
    if err.is_none() && warn.is_none() {
      response = response.on_hover_text(title.as_str());
    } else {
      let mut lines = vec![title.clone()];
      if let Some(err) = err {
        lines.push(format!("Error: {err}"));
      }
      if let Some(warn) = warn {
        lines.push(format!("Warning: {warn}"));
      }
      response = response.on_hover_text(lines.join("\n"));
    }
  }
  let a11y_label = tab_a11y_label(title.as_str(), tab.loading, err.is_some(), warn.is_some());
  response.widget_info({
    let a11y_label = a11y_label.clone();
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label.clone())
  });
  super::show_tooltip_on_focus(ui, &response, title.as_str());

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

  // Micro-interaction: fade the active tab background in/out instead of snapping.
  let active_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("active"),
    is_active,
    motion.durations.tab_underline,
  );

  let inactive_bg = lerp_color(
    visuals.widgets.inactive.bg_fill,
    visuals.widgets.hovered.bg_fill,
    hover_t,
  );
  let bg = lerp_color(inactive_bg, visuals.widgets.active.bg_fill, active_t);
  let rounding = visuals.widgets.inactive.rounding;
  {
    let painter = paint_ui.painter();
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
    if paint_ui.is_rect_visible(icon_rect) {
      let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
      paint_ui
        .painter()
        .image(tex_id, icon_rect, uv, Color32::WHITE);
    }
  } else {
    placeholder_favicon(paint_ui.painter(), icon_rect, &visuals);
    // Show a small deterministic glyph so blank favicons are still distinguishable at a glance.
    let glyph = title
      .trim()
      .chars()
      .next()
      .map(|ch| ch.to_ascii_uppercase().to_string())
      .unwrap_or_else(|| "?".to_string());
    paint_ui.painter().text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(12.0),
      with_alpha(visuals.text_color(), 0.75),
    );
  }

  if tab.loading {
    // Spinner overlay around the favicon.
    let time = if motion.enabled {
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
      visuals.text_color(),
    );
  }

  let err_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_error"),
    err.is_some(),
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    warn.is_some(),
    motion.durations.progress_fade,
  );
  paint_tab_status_badges(paint_ui.painter(), icon_rect, &visuals, err_t, warn_t);

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
          // When the close icon is hidden (non-active tab, not hovered), avoid stealing clicks/focus
          // from the tab itself.
          Sense::hover()
        },
      )
      .on_hover_text("Close tab (Ctrl/Cmd+W)");
    close_resp.widget_info({
      let label = format!("{}: {}", BrowserIcon::CloseTab.a11y_label(), title);
      move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
    });
    super::show_tooltip_on_focus(ui, &close_resp, "Close tab (Ctrl/Cmd+W)");
    close_clicked = close_reveal_target && close_resp.clicked();

    // Micro-interaction: fade close button hover fill in/out.
    let close_rounding =
      egui::Rounding::same((visuals.widgets.inactive.rounding.nw * 0.8).clamp(4.0, 6.0));
    let close_hover_t = motion.animate_bool(
      ui.ctx(),
      close_id.with("hover"),
      close_resp.hovered(),
      motion.durations.hover_fade,
    );
    if close_hover_t > 0.0 {
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
    let label = {
      let mut text = egui::RichText::new(title);
      if is_active {
        text = text.strong();
      }
      egui::Label::new(text).truncate(true).wrap(false)
    };
    let _ = paint_ui.put(title_rect, label);
  }

  // Input semantics.
  if response.clicked_by(egui::PointerButton::Secondary) {
    if let Some(pos) = response
      .interact_pointer_pos()
      .or_else(|| ui.input(|i| i.pointer.hover_pos()))
    {
      chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
        tab_id: tab.id,
        anchor_points: (pos.x, pos.y),
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
  if response.double_clicked() {
    if can_close_tabs {
      chrome.open_tab_context_menu = None;
      chrome.tab_context_menu_rect = None;
      return (tab_rect, response, Some(ChromeAction::CloseTab(tab.id)));
    }
  }
  if response.clicked_by(egui::PointerButton::Middle) {
    if can_close_tabs {
      chrome.open_tab_context_menu = None;
      chrome.tab_context_menu_rect = None;
      return (tab_rect, response, Some(ChromeAction::CloseTab(tab.id)));
    }
  } else if response.clicked() {
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
  tab: &BrowserTabState,
  is_active: bool,
  can_close_tabs: bool,
  favicon_tex: Option<egui::TextureId>,
  chrome: &mut ChromeState,
  focus_ring: FocusRingStyle,
) -> (Rect, Response, Option<ChromeAction>) {
  let (_, tab_rect) = ui.allocate_space(Vec2::new(PINNED_TAB_WIDTH, TAB_HEIGHT));
  let tab_id = ui.make_persistent_id(("tab_strip_tab", tab.id));
  let title = tab.display_title();
  let (err, warn) = tab_status_messages(tab);
  let mut response = ui.interact(tab_rect, tab_id, Sense::click_and_drag());
  if response.hovered() {
    if err.is_none() && warn.is_none() {
      response = response.on_hover_text(title.as_str());
    } else {
      let mut lines = vec![title.clone()];
      if let Some(err) = err {
        lines.push(format!("Error: {err}"));
      }
      if let Some(warn) = warn {
        lines.push(format!("Warning: {warn}"));
      }
      response = response.on_hover_text(lines.join("\n"));
    }
  }
  let a11y_label = tab_a11y_label(title.as_str(), tab.loading, err.is_some(), warn.is_some());
  response.widget_info({
    let a11y_label = a11y_label.clone();
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label.clone())
  });
  super::show_tooltip_on_focus(ui, &response, title.as_str());

  let visuals = ui.style().visuals.clone();

  // Micro-interaction: fade hover highlight in/out (active tabs ignore hover).
  let hover_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("hover"),
    response.hovered() && !is_active,
    motion.durations.hover_fade,
  );

  // Micro-interaction: fade the active tab background in/out instead of snapping.
  let active_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("active"),
    is_active,
    motion.durations.tab_underline,
  );

  let inactive_bg = lerp_color(
    visuals.widgets.inactive.bg_fill,
    visuals.widgets.hovered.bg_fill,
    hover_t,
  );
  let bg = lerp_color(inactive_bg, visuals.widgets.active.bg_fill, active_t);
  let rounding = visuals.widgets.inactive.rounding;
  ui.painter().rect_filled(tab_rect, rounding, bg);

  // Favicon (centered).
  let icon_min = Pos2::new(
    tab_rect.center().x - ICON_SIZE * 0.5,
    tab_rect.center().y - ICON_SIZE * 0.5,
  );
  let icon_rect = Rect::from_min_size(icon_min, Vec2::splat(ICON_SIZE));
  if let Some(tex_id) = favicon_tex {
    if ui.is_rect_visible(icon_rect) {
      let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
      ui.painter().image(tex_id, icon_rect, uv, Color32::WHITE);
    }
  } else {
    placeholder_favicon(ui.painter(), icon_rect, &visuals);
    let glyph = title
      .trim()
      .chars()
      .next()
      .map(|ch| ch.to_ascii_uppercase().to_string())
      .unwrap_or_else(|| "?".to_string());
    ui.painter().text(
      icon_rect.center(),
      Align2::CENTER_CENTER,
      glyph,
      FontId::proportional(14.0),
      visuals.text_color(),
    );
  }

  if tab.loading {
    // Spinner overlay around the favicon.
    let time = if motion.enabled {
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
    err.is_some(),
    motion.durations.progress_fade,
  );
  let warn_t = motion.animate_bool(
    ui.ctx(),
    tab_id.with("status_warning"),
    warn.is_some(),
    motion.durations.progress_fade,
  );
  paint_tab_status_badges(ui.painter(), icon_rect, &visuals, err_t, warn_t);

  // Input semantics.
  if response.clicked_by(egui::PointerButton::Secondary) {
    if let Some(pos) = response
      .interact_pointer_pos()
      .or_else(|| ui.input(|i| i.pointer.hover_pos()))
    {
      chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
        tab_id: tab.id,
        anchor_points: (pos.x, pos.y),
      });
      chrome.tab_context_menu_rect = None;
    }
    return (tab_rect, response, None);
  }
  if response.double_clicked() {
    if can_close_tabs {
      chrome.open_tab_context_menu = None;
      chrome.tab_context_menu_rect = None;
      return (tab_rect, response, Some(ChromeAction::CloseTab(tab.id)));
    }
  }
  if response.clicked_by(egui::PointerButton::Middle) {
    if can_close_tabs {
      chrome.open_tab_context_menu = None;
      chrome.tab_context_menu_rect = None;
      return (tab_rect, response, Some(ChromeAction::CloseTab(tab.id)));
    }
  } else if response.clicked() {
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

  // Defensive: if the dragged tab was closed mid-drag, clear the drag state.
  if let Some(dragging_tab_id) = app.chrome.dragging_tab_id {
    if app.tab(dragging_tab_id).is_none() {
      app.chrome.clear_tab_drag();
    }
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

  let pinned_content_width = if pinned_count == 0 {
    0.0
  } else {
    (pinned_count as f32) * PINNED_TAB_WIDTH + (pinned_count.saturating_sub(1) as f32) * TAB_GAP
  };
  let mut segment_gap = if pinned_count > 0 && unpinned_count > 0 {
    TAB_GAP
  } else {
    0.0
  };

  let mut pinned_viewport_width = if pinned_count == 0 {
    0.0
  } else if unpinned_count == 0 {
    tabs_viewport_width
  } else {
    let max_by_fraction = tabs_viewport_width * PINNED_VIEWPORT_MAX_FRACTION;
    let max_by_unpinned = (tabs_viewport_width - MIN_UNPINNED_VIEWPORT - segment_gap).max(0.0);
    // Ensure pinned tabs remain discoverable even in narrow strips by keeping at least one pinned
    // tab width when possible.
    pinned_content_width
      .min(max_by_fraction.min(max_by_unpinned))
      .max(PINNED_TAB_WIDTH.min(tabs_viewport_width))
      .min(tabs_viewport_width)
  };

  // If the gap would fully consume what little space remains (e.g. very narrow windows), drop it
  // so neither segment collapses to a 0-width viewport.
  if pinned_count > 0 && unpinned_count > 0 && segment_gap > 0.0 {
    let remaining = tabs_viewport_width - pinned_viewport_width - segment_gap;
    if remaining <= 0.0 {
      segment_gap = 0.0;
      let max_by_fraction = tabs_viewport_width * PINNED_VIEWPORT_MAX_FRACTION;
      let max_by_unpinned = (tabs_viewport_width - MIN_UNPINNED_VIEWPORT - segment_gap).max(0.0);
      pinned_viewport_width = pinned_content_width
        .min(max_by_fraction.min(max_by_unpinned))
        .max(PINNED_TAB_WIDTH.min(tabs_viewport_width))
        .min(tabs_viewport_width);
    }
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

  // Tab group collapse/expand animation state. This is computed up front so sizing + layout can
  // account for groups that are mid-transition.
  let mut group_expand_t: HashMap<TabGroupId, f32> = HashMap::new();
  let mut group_animating = false;
  {
    for tab in app.tabs.iter().skip(pinned_len) {
      let Some(group_id) = tab.group else {
        continue;
      };
      if group_expand_t.contains_key(&group_id) {
        continue;
      }
      let collapsed = app.tab_groups.get(&group_id).is_some_and(|g| g.collapsed);
      let id = ui.make_persistent_id(("tab_group_expand", group_id.0));
      let t = motion.animate_bool(
        ui.ctx(),
        id,
        !collapsed,
        motion.durations.tab_group_collapse,
      );
      let target = if collapsed { 0.0 } else { 1.0 };
      if motion.enabled && ui.ctx().style().animation_time > 0.0 && (t - target).abs() > 0.001 {
        group_animating = true;
      }
      group_expand_t.insert(group_id, t);
    }
  }
  if group_animating {
    ui.ctx().request_repaint();
  }

  let mut visible_unpinned_count = 0usize;
  let mut group_chip_count = 0usize;
  let mut group_chip_total_width = 0.0f32;
  {
    let mut idx = pinned_len;
    while idx < app.tabs.len() {
      if let Some(group_id) = app.tabs[idx].group {
        if let Some(group) = app.tab_groups.get(&group_id) {
          let is_first = idx == pinned_len || app.tabs[idx - 1].group != Some(group_id);
          if is_first {
            let title = if group.title.trim().is_empty() {
              "Group"
            } else {
              group.title.as_str()
            };
            group_chip_total_width += group_chip_width(ui, title);
            group_chip_count += 1;
          }

          let t = group_expand_t
            .get(&group_id)
            .copied()
            .unwrap_or(if group.collapsed { 0.0 } else { 1.0 });

          if group.collapsed && t <= 0.001 {
            while idx < app.tabs.len() && app.tabs[idx].group == Some(group_id) {
              idx += 1;
            }
            continue;
          }
        }
      }
      visible_unpinned_count += 1;
      idx += 1;
    }
  }
  let total_item_count = visible_unpinned_count.saturating_add(group_chip_count);
  let sizing = compute_tab_strip_sizing_with_fixed_width(
    unpinned_viewport_width,
    visible_unpinned_count,
    group_chip_total_width,
    total_item_count.saturating_sub(1),
  );

  let mut ops: Vec<TabStripOp> = Vec::new();

  #[cfg(test)]
  let mut tab_rects_for_test: Vec<Rect> = Vec::new();

  let mut pinned_tab_rects_for_drag: Vec<(TabId, Rect)> = Vec::with_capacity(pinned_count);
  let mut unpinned_tab_rects_for_drag: Vec<(TabId, Rect)> = Vec::with_capacity(unpinned_count);
  let mut dragged_tab_rect: Option<Rect> = None;

  let mut active_tab_rect: Option<Rect> = None;
  let mut active_tab_is_pinned = false;
  let mut pinned_scroll_offset_x: f32 = 0.0;
  let mut pinned_max_scroll_x: f32 = 0.0;
  let mut scroll_offset_x: f32 = 0.0;
  let mut unpinned_max_scroll_x: f32 = 0.0;

  if tabs_viewport_width > 0.0 {
    if pinned_count > 0 && pinned_viewport_rect.width() > 0.0 {
      let mut pinned_ui = ui.child_ui(
        pinned_viewport_rect,
        egui::Layout::left_to_right(egui::Align::Center),
      );
      pinned_ui.set_clip_rect(pinned_viewport_rect);
      let mut restore_scroll_delta: Option<Vec2> = None;
      // Match the unpinned segment ergonomics: treat vertical wheel scroll as horizontal scroll
      // while the pointer is over the pinned strip.
      let pointer_over_strip = pinned_ui.input(|i| {
        i.pointer
          .hover_pos()
          .is_some_and(|pos| pinned_viewport_rect.contains(pos))
      });
      if pointer_over_strip {
        let has_vertical_scroll = pinned_ui.input(|i| i.scroll_delta.y.abs() > 0.0);
        if has_vertical_scroll {
          pinned_ui.ctx().input_mut(|i| {
            restore_scroll_delta = Some(i.scroll_delta);
            i.scroll_delta = Vec2::new(i.scroll_delta.x + i.scroll_delta.y, 0.0);
          });
        }
      }

      let scroll_output = egui::ScrollArea::horizontal()
        .id_source("tab_strip_pinned_scroll")
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut pinned_ui, |ui| {
          ui.spacing_mut().item_spacing = Vec2::new(TAB_GAP, 0.0);
          ui.horizontal(|ui| {
            let (tabs, chrome) = (&app.tabs, &mut app.chrome);
            for idx in 0..pinned_count {
              let tab = &tabs[idx];
              let tab_id = tab.id;
              let is_active = active_id == Some(tab_id);
              let favicon_tex = favicon_for_tab(tab_id);
              let is_dragged = chrome.dragging_tab_id == Some(tab_id);
              let (tab_rect, tab_response, maybe_action) = if is_dragged {
                // While dragging, keep layout stable but don't paint the tab in-flow.
                let (_, rect) = ui.allocate_space(Vec2::new(PINNED_TAB_WIDTH, TAB_HEIGHT));
                (rect, None, None)
              } else {
                let (rect, resp, action) = pinned_tab_ui(
                  ui,
                  motion,
                  tab,
                  is_active,
                  can_close_tabs,
                  favicon_tex,
                  chrome,
                  focus_ring,
                );
                (rect, Some(resp), action)
              };
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
              pinned_tab_rects_for_drag.push((tab_id, tab_rect));
              let is_close_action = maybe_action
                .as_ref()
                .is_some_and(|action| matches!(action, ChromeAction::CloseTab(_)));
              if !is_close_action {
                if let Some(tab_response) = &tab_response {
                  if tab_response.drag_started() && chrome.dragging_tab_id.is_none() {
                    chrome.dragging_tab_id = Some(tab_id);
                    let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                    chrome.drag_start_pointer_pos = pointer_pos;
                    if let Some(pointer_pos) = pointer_pos {
                      ui.ctx()
                        .data_mut(|d| d.insert_temp(drag_offset_id(), tab_rect.min - pointer_pos));
                    }
                  }
                }
              }
              if chrome.dragging_tab_id == Some(tab_id) {
                dragged_tab_rect = Some(tab_rect);
              }
              if let Some(action) = maybe_action {
                actions.push(action);
              }
            }
          });
        });
      let mut scroll_state = scroll_output.state;
      pinned_scroll_offset_x = scroll_state.offset.x;
      pinned_max_scroll_x =
        (scroll_output.content_size.x - scroll_output.inner_rect.width()).max(0.0);
      if let Some(scroll_delta) = restore_scroll_delta {
        pinned_ui.ctx().input_mut(|i| {
          i.scroll_delta = scroll_delta;
        });
      }

      // Auto-scroll pinned strip while drag-reordering a pinned tab.
      if pinned_max_scroll_x > 0.5 {
        if let (Some(dragging_tab_id), Some(pointer_pos)) = (
          app.chrome.dragging_tab_id,
          ui.input(|i| i.pointer.interact_pos()),
        ) {
          let dragging_is_pinned = app.tab(dragging_tab_id).is_some_and(|tab| tab.pinned);
          if dragging_is_pinned
            && pointer_pos.y >= pinned_viewport_rect.top()
            && pointer_pos.y <= pinned_viewport_rect.bottom()
          {
            let dt = ui.ctx().input(|i| i.stable_dt).clamp(0.0, 0.1);
            let delta_x = drag_autoscroll_delta_x(pointer_pos, pinned_viewport_rect, dt);
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
    }

    if unpinned_count > 0 && unpinned_viewport_rect.width() > 0.0 {
      let mut unpinned_ui = ui.child_ui(
        unpinned_viewport_rect,
        egui::Layout::left_to_right(egui::Align::Center),
      );
      unpinned_ui.set_clip_rect(unpinned_viewport_rect);

      let mut restore_scroll_delta: Option<Vec2> = None;
      // Browser-like ergonomics: treat vertical wheel scrolling as horizontal scrolling when the
      // pointer is over the tab strip (so users don't need a trackpad horizontal gesture).
      let pointer_over_strip = unpinned_ui.input(|i| {
        i.pointer
          .hover_pos()
          .is_some_and(|pos| unpinned_viewport_rect.contains(pos))
      });
      if pointer_over_strip {
        let has_vertical_scroll = unpinned_ui.input(|i| i.scroll_delta.y.abs() > 0.0);
        if has_vertical_scroll {
          unpinned_ui.ctx().input_mut(|i| {
            restore_scroll_delta = Some(i.scroll_delta);
            i.scroll_delta = Vec2::new(i.scroll_delta.x + i.scroll_delta.y, 0.0);
          });
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
            let mut idx = pinned_len;
            let mut first_item = true;
            // Scale gaps based on the visibility of adjacent items so collapsing tabs don't leave
            // behind fixed `TAB_GAP` holes. We use `min(prev, curr)` so:
            // - chip<->tab gaps shrink with the tab during collapse/expand
            // - chip<->chip and chip<->ungrouped tab gaps remain at full size
            let mut prev_item_scale: f32 = 1.0;

            let mut add_gap =
              |ui: &mut egui::Ui, first_item: &mut bool, prev_scale: f32, curr_scale: f32| {
                if *first_item {
                  *first_item = false;
                  return;
                }
                let scale = prev_scale.min(curr_scale).clamp(0.0, 1.0);
                let gap = (TAB_GAP * scale).max(0.0);
                if gap > 0.0 {
                  ui.add_space(gap);
                }
              };
            while idx < app.tabs.len() {
              let tab_id = app.tabs[idx].id;
              let tab_group = app.tabs[idx].group;

              if let Some(group_id) = tab_group {
                let is_first = idx == pinned_len || app.tabs[idx - 1].group != Some(group_id);
                if is_first {
                  add_gap(ui, &mut first_item, prev_item_scale, 1.0);
                  group_chip_ui(ui, motion, app, group_id, &mut ops, focus_ring);
                  prev_item_scale = 1.0;
                }

                let collapsed = app.tab_groups.get(&group_id).is_some_and(|g| g.collapsed);
                let group_t = group_expand_t
                  .get(&group_id)
                  .copied()
                  .unwrap_or(1.0)
                  .clamp(0.0, 1.0);
                if collapsed && group_t <= 0.001 {
                  // Fully collapsed: hide all member tabs (but keep the chip visible).
                  while idx < app.tabs.len() && app.tabs[idx].group == Some(group_id) {
                    idx += 1;
                  }
                  continue;
                }

                add_gap(ui, &mut first_item, prev_item_scale, group_t);
                let interactive = !collapsed && group_t > 0.95;
                let tab_width = sizing.tab_width * group_t;
                let is_active = active_id == Some(tab_id);
                let favicon_tex = favicon_for_tab(tab_id);
                let (tab_rect, tab_response, maybe_action) = {
                  let tab = &app.tabs[idx];
                  let group_border = tab
                    .group
                    .and_then(|gid| app.tab_groups.get(&gid).map(|g| group_color_egui(g.color)));
                  tab_ui(
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
                  )
                };
                if is_active {
                  active_tab_rect = Some(tab_rect);
                  active_tab_is_pinned = false;
                }
                if is_active && active_changed {
                  tab_response.scroll_to_me(Some(egui::Align::Center));
                }
                #[cfg(test)]
                tab_rects_for_test.push(tab_rect);

                if interactive {
                  unpinned_tab_rects_for_drag.push((tab_id, tab_rect));
                  if tab_response.drag_started() && app.chrome.dragging_tab_id.is_none() {
                    app.chrome.dragging_tab_id = Some(tab_id);
                    app.chrome.drag_start_pointer_pos = ui.input(|i| i.pointer.interact_pos());
                  }
                  if app.chrome.dragging_tab_id == Some(tab_id) {
                    dragged_tab_rect = Some(tab_rect);
                  }
                }

                if let Some(action) = maybe_action {
                  actions.push(action);
                }

                prev_item_scale = group_t;
                idx += 1;
                continue;
              }

              add_gap(ui, &mut first_item, prev_item_scale, 1.0);
              prev_item_scale = 1.0;
              let is_active = active_id == Some(tab_id);
              let favicon_tex = favicon_for_tab(tab_id);
              let is_dragged = app.chrome.dragging_tab_id == Some(tab_id);
              let (tab_rect, tab_response, maybe_action) = if is_dragged {
                // While dragging, keep layout stable but don't paint the tab in-flow.
                let (_, rect) = ui.allocate_space(Vec2::new(sizing.tab_width, TAB_HEIGHT));
                (rect, None, None)
              } else {
                let tab = &app.tabs[idx];
                let group_border = tab
                  .group
                  .and_then(|gid| app.tab_groups.get(&gid).map(|g| group_color_egui(g.color)));
                let (rect, resp, action) = tab_ui(
                  ui,
                  motion,
                  tab,
                  is_active,
                  true,
                  can_close_tabs,
                  sizing.tab_width,
                  favicon_tex,
                  &mut app.chrome,
                  focus_ring,
                  group_border,
                );
                (rect, Some(resp), action)
              };
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

              unpinned_tab_rects_for_drag.push((tab_id, tab_rect));
              let is_close_action = maybe_action
                .as_ref()
                .is_some_and(|action| matches!(action, ChromeAction::CloseTab(_)));
              if !is_close_action {
                if let Some(tab_response) = &tab_response {
                  if tab_response.drag_started() && app.chrome.dragging_tab_id.is_none() {
                    app.chrome.dragging_tab_id = Some(tab_id);
                    let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                    app.chrome.drag_start_pointer_pos = pointer_pos;
                    if let Some(pointer_pos) = pointer_pos {
                      ui.ctx()
                        .data_mut(|d| d.insert_temp(drag_offset_id(), tab_rect.min - pointer_pos));
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

              idx += 1;
            }
          });
        });
      let mut scroll_state = scroll_output.state;
      scroll_offset_x = scroll_state.offset.x;
      unpinned_max_scroll_x =
        (scroll_output.content_size.x - scroll_output.inner_rect.width()).max(0.0);
      if let Some(scroll_delta) = restore_scroll_delta {
        unpinned_ui.ctx().input_mut(|i| {
          i.scroll_delta = scroll_delta;
        });
      }

      // Use the scroll area's actual widget id for programmatic state updates, rather than
      // assuming how `id_source` is transformed internally by egui.
      let scroll_state_id = scroll_output.id;

      // While dragging an unpinned tab, auto-scroll the overflowing scroll area when the pointer is
      // near the left/right edge of the unpinned viewport (standard browser UX).
      if unpinned_max_scroll_x > 0.5 {
        if let (Some(dragging_tab_id), Some(pointer_pos)) = (
          app.chrome.dragging_tab_id,
          ui.input(|i| i.pointer.interact_pos()),
        ) {
          let dragging_is_unpinned = app.tab(dragging_tab_id).is_some_and(|tab| !tab.pinned);
          if dragging_is_unpinned
            && pointer_pos.y >= unpinned_viewport_rect.top()
            && pointer_pos.y <= unpinned_viewport_rect.bottom()
          {
            let dt = ui.ctx().input(|i| i.stable_dt).clamp(0.0, 0.1);
            let delta_x = drag_autoscroll_delta_x(pointer_pos, unpinned_viewport_rect, dt);
            if delta_x != 0.0 {
              let prev = scroll_state.offset.x;
              let next = (prev + delta_x).clamp(0.0, unpinned_max_scroll_x);
              if (next - prev).abs() > 0.01 {
                scroll_state.offset.x = next;
                scroll_state.store(ui.ctx(), scroll_state_id);
                ui.ctx().request_repaint();
              }
            }
          }
        }
      }
    }
  }

  // Visual separator between pinned and unpinned tabs.
  if pinned_count > 0
    && unpinned_count > 0
    && pinned_viewport_rect.width() > 0.0
    && unpinned_viewport_rect.width() > 0.0
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

  // Edge fades: scrollbars are hidden, so use subtle fades as the overflow affordance.
  if pinned_count > 0 && pinned_viewport_rect.width() > 0.0 {
    paint_scroll_edge_fades(
      ui,
      pinned_viewport_rect,
      pinned_scroll_offset_x,
      pinned_max_scroll_x,
    );
  }
  if unpinned_viewport_rect.width() > 0.0 {
    paint_scroll_edge_fades(
      ui,
      unpinned_viewport_rect,
      scroll_offset_x,
      unpinned_max_scroll_x,
    );
  }

  // Micro-interaction: animate the active tab underline position/width.
  if let Some(active_rect) = active_tab_rect {
    let underline_id = ui.make_persistent_id("tab_strip_active_underline");
    let pinned_offset = unpinned_viewport_rect.min.x - tabs_rect.min.x;
    // Animate in a unified content coordinate space so the underline tracks scroll (unpinned tabs)
    // without lag, while still supporting pinned tabs which sit outside the scroll area.
    let target_center_content_x = if active_tab_is_pinned {
      active_rect.center().x - tabs_rect.min.x
    } else {
      pinned_offset + (active_rect.center().x - unpinned_viewport_rect.min.x + scroll_offset_x)
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
      tabs_rect.min.x + center_content_x
    } else {
      unpinned_viewport_rect.min.x + (center_content_x - pinned_offset) - scroll_offset_x
    };
    let x0 = center_screen_x - width * 0.5;
    let x1 = center_screen_x + width * 0.5;
    let y = active_rect.max.y - ACTIVE_UNDERLINE_HEIGHT * 0.5;
    ui.painter().with_clip_rect(tabs_rect).line_segment(
      [Pos2::new(x0, y), Pos2::new(x1, y)],
      Stroke::new(ACTIVE_UNDERLINE_HEIGHT, ui.visuals().selection.stroke.color),
    );
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

    let (tab_rects_for_drag, group_start_index, group_clip_rect) = if dragging_is_pinned {
      (&pinned_tab_rects_for_drag, 0usize, pinned_viewport_rect)
    } else {
      (
        &unpinned_tab_rects_for_drag,
        pinned_count,
        unpinned_viewport_rect,
      )
    };

    // Used for placeholder + preview styling.
    let dragged_group_color = app
      .tab(dragging_tab_id)
      .and_then(|t| t.group)
      .and_then(|gid| app.tab_groups.get(&gid))
      .map(|g| group_color_egui(g.color));

    let mut insertion_index: Option<usize> = None;
    let mut target_index: Option<usize> = None;
    let mut insertion_changed = false;

    if tab_rects_for_drag.len() >= 2 {
      // Determine the insertion point by comparing the pointer x coordinate against the centers of
      // each *other* tab.
      let mut idx: usize = 0;
      for (tab_id, rect) in tab_rects_for_drag {
        if *tab_id == dragging_tab_id {
          continue;
        }
        if pos.x < rect.center().x {
          break;
        }
        idx += 1;
      }
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
      let pulse_target = if motion.enabled && insertion_changed {
        1.0
      } else {
        0.0
      };
      let pulse_t = motion.animate_f32(
        ui.ctx(),
        egui::Id::new("tab_strip_drag_gap_pulse"),
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

      let drop_id = egui::Id::new("tab_strip_drop_indicator");
      let drop_x = motion.animate_f32(
        ui.ctx(),
        drop_id.with("x"),
        drop_x,
        motion.durations.hover_fade,
      );
      let target_alpha = if motion.enabled && insertion_changed {
        1.0
      } else if motion.enabled {
        0.7
      } else {
        1.0
      };
      let alpha = motion.animate_f32(
        ui.ctx(),
        drop_id.with("a"),
        target_alpha,
        motion.durations.hover_fade,
      );

      let stroke = Stroke::new(
        2.0,
        with_alpha(ui.visuals().widgets.active.bg_stroke.color, alpha),
      );
      let y1 = tab_strip_rect.top() + 1.0;
      let y2 = tab_strip_rect.bottom() - 1.0;
      ui.painter()
        .with_clip_rect(group_clip_rect)
        .line_segment([Pos2::new(drop_x, y1), Pos2::new(drop_x, y2)], stroke);
    }

    // Floating preview.
    let preview_size = dragged_tab_rect.map(|r| r.size()).unwrap_or_else(|| {
      Vec2::new(
        if dragging_is_pinned {
          PINNED_TAB_WIDTH
        } else {
          sizing.tab_width
        },
        TAB_HEIGHT,
      )
    });
    let drag_offset = ui
      .ctx()
      .data(|d| d.get_temp::<Vec2>(drag_offset_id()))
      .or_else(|| dragged_tab_rect.map(|r| r.min - pos))
      .unwrap_or(Vec2::new(-preview_size.x * 0.5, -preview_size.y * 0.5));
    let lift = Vec2::new(0.0, -DRAG_PREVIEW_LIFT_Y);
    let mut preview_rect = Rect::from_min_size(pos + drag_offset + lift, preview_size);
    if motion.enabled {
      preview_rect = Rect::from_center_size(
        preview_rect.center(),
        preview_rect.size() * DRAG_PREVIEW_SCALE,
      );
    }

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
          paint_popup_shadow(ui.painter(), preview_rect, rounding, visuals.popup_shadow);

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
      app.chrome.drag_target_index = Some(target_index);
      // Apply the reorder immediately while dragging (standard browser behaviour).
      app.drag_reorder_tab(dragging_tab_id, target_index);
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
      icon_button(ui, BrowserIcon::NewTab, "New tab (Ctrl/Cmd+T)", true)
    })
    .inner;
  new_tab_resp.widget_info(|| {
    egui::WidgetInfo::labeled(egui::WidgetType::Button, BrowserIcon::NewTab.a11y_label())
  });
  if new_tab_resp.clicked() {
    actions.push(ChromeAction::NewTab);
  }

  #[cfg(test)]
  {
    store_test_layout(ui.ctx(), strip_rect, tab_rects_for_test);
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

  #[test]
  fn tab_a11y_label_formats_loading_error_warning_states() {
    let title = "Example title";
    let cases = [
      (false, false, false, "Example title"),
      (true, false, false, "Example title (loading)"),
      (false, true, false, "Example title (error)"),
      (false, false, true, "Example title (warning)"),
      (true, true, false, "Example title (loading, error)"),
      (true, false, true, "Example title (loading, warning)"),
      (false, true, true, "Example title (error, warning)"),
      (true, true, true, "Example title (loading, error, warning)"),
    ];
    for (loading, err, warn, expected) in cases {
      assert_eq!(tab_a11y_label(title, loading, err, warn), expected);
    }
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
    assert!((sizing_tabs.total_content_width - sizing_extras.total_content_width).abs() < f32::EPSILON);
  }

  #[test]
  fn sizing_accounts_for_extra_item_width() {
    let available = 600.0;
    let tabs: usize = 3;
    let chip_width = 120.0;

    let sizing_no_chips = compute_tab_strip_sizing(available, tabs);
    assert!((sizing_no_chips.tab_width - 196.0).abs() < 0.01);

    // 3 tabs + 1 chip => 4 items => 3 gaps.
    let sizing_with_chip = compute_tab_strip_sizing_with_fixed_width(available, tabs, chip_width, tabs);
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
}
