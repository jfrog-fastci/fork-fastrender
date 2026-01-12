use crate::ui::browser_app::{BrowserAppState, BrowserTabState};
use crate::ui::a11y;
use crate::ui::messages::TabId;
use crate::ui::motion::UiMotion;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use super::ChromeAction;

const TAB_STRIP_HEIGHT: f32 = 34.0;
const TAB_HEIGHT: f32 = 32.0;
const TAB_MIN_WIDTH: f32 = 140.0;
const TAB_MAX_WIDTH: f32 = 240.0;
const TAB_GAP: f32 = 6.0;
const TAB_PADDING_X: f32 = 10.0;
const CONTROL_BUTTON_SIZE: f32 = 28.0;
const ICON_SIZE: f32 = 16.0;
const ICON_GAP: f32 = 8.0;
const CLOSE_BUTTON_SIZE: f32 = 28.0;
const ACTIVE_UNDERLINE_HEIGHT: f32 = 2.0;

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TabStripSizing {
  pub tab_width: f32,
  pub overflow: bool,
  pub total_content_width: f32,
}

/// Pure sizing logic for the tab strip.
///
/// This keeps the strip single-row by shrinking tabs down to `TAB_MIN_WIDTH`, and reports whether
/// tabs will overflow the available viewport width at that minimum.
pub(super) fn compute_tab_strip_sizing(available_width: f32, tab_count: usize) -> TabStripSizing {
  if tab_count == 0 {
    return TabStripSizing {
      tab_width: 0.0,
      overflow: false,
      total_content_width: 0.0,
    };
  }

  let available_width = if available_width.is_finite() {
    available_width.max(0.0)
  } else {
    0.0
  };

  let gaps = TAB_GAP * (tab_count.saturating_sub(1) as f32);
  let ideal_width = ((available_width - gaps) / (tab_count as f32)).max(0.0);
  let tab_width = ideal_width.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH);
  let total_content_width = (tab_width * tab_count as f32) + gaps;
  let overflow = total_content_width > available_width + 0.5;

  TabStripSizing {
    tab_width,
    overflow,
    total_content_width,
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
      Stroke::new(2.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)),
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

fn tab_ui(
  ui: &mut egui::Ui,
  motion: UiMotion,
  tab: &BrowserTabState,
  is_active: bool,
  can_close_tabs: bool,
  tab_width: f32,
  favicon_tex: Option<egui::TextureId>,
) -> (Rect, Option<ChromeAction>) {
  let (_, tab_rect) = ui.allocate_space(Vec2::new(tab_width, TAB_HEIGHT));
  let tab_id = ui.make_persistent_id(("tab_strip_tab", tab.id));
  let title = tab.display_title();
  let response = ui
    .interact(tab_rect, tab_id, Sense::click())
    .on_hover_text(title.as_str());
  response.widget_info({
    let title = title.clone();
    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, title.clone())
  });

  let visuals = ui.style().visuals.clone();

  // Micro-interaction: fade hover highlight in/out for inactive tabs.
  let hover_t = if is_active {
    0.0
  } else {
    motion.animate_bool(
      ui.ctx(),
      tab_id.with("hover"),
      response.hovered(),
      motion.durations.hover_fade,
    )
  };

  let bg = if is_active {
    visuals.widgets.active.bg_fill
  } else {
    lerp_color(visuals.widgets.inactive.bg_fill, visuals.widgets.hovered.bg_fill, hover_t)
  };
  let rounding = visuals.widgets.inactive.rounding;
  {
    let painter = ui.painter();
    painter.rect_filled(tab_rect, rounding, bg);
  }

  // Favicon.
  let icon_min = Pos2::new(tab_rect.min.x + TAB_PADDING_X, tab_rect.center().y - ICON_SIZE * 0.5);
  let icon_rect = Rect::from_min_size(icon_min, Vec2::splat(ICON_SIZE));
  if let Some(tex_id) = favicon_tex {
    let _ = ui.put(
      icon_rect,
      egui::Image::new((tex_id, icon_rect.size())).sense(Sense::hover()),
    );
  } else {
    placeholder_favicon(ui.painter(), icon_rect, &visuals);
  }

  if tab.loading {
    // Spinner overlay around the favicon.
    ui.ctx().request_repaint();
    let time = ui.input(|i| i.time);
    paint_spinner(ui.painter(), icon_rect.expand(2.0), time, visuals.text_color());
  }

  // Close button (only when more than one tab exists).
  let mut close_clicked = false;
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
    let close_id = ui.make_persistent_id(("tab_strip_close", tab.id));
    let close_resp = ui
      .interact(close_rect, close_id, Sense::click())
      .on_hover_text("Close tab (Ctrl/Cmd+W)");
    close_resp.widget_info({
      let label = format!("{}: {}", a11y::ChromeIconButton::CloseTab.label(), title);
      move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
    });
    close_clicked = close_resp.clicked();

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
      ui.painter().rect_filled(
        close_rect,
        close_rounding,
        with_alpha(visuals.widgets.hovered.bg_fill.gamma_multiply(0.85), close_hover_t),
      );
    }

    ui.painter().text(
      close_rect.center(),
      Align2::CENTER_CENTER,
      "×",
      FontId::proportional(16.0),
      visuals.text_color(),
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

  // Input semantics.
  if close_clicked && can_close_tabs {
    return (tab_rect, Some(ChromeAction::CloseTab(tab.id)));
  }
  if response.clicked_by(egui::PointerButton::Middle) {
    if can_close_tabs {
      return (tab_rect, Some(ChromeAction::CloseTab(tab.id)));
    }
  } else if response.clicked() {
    return (tab_rect, Some(ChromeAction::ActivateTab(tab.id)));
  }

  (tab_rect, None)
}

pub(super) fn tab_strip_ui(
  ui: &mut egui::Ui,
  app: &BrowserAppState,
  favicon_for_tab: &mut impl FnMut(TabId) -> Option<egui::TextureId>,
  motion: UiMotion,
) -> Vec<ChromeAction> {
  let mut actions = Vec::new();

  let strip_width = ui.available_width().max(0.0);
  let (_, strip_rect) = ui.allocate_space(Vec2::new(strip_width, TAB_STRIP_HEIGHT));
  let button_size = CONTROL_BUTTON_SIZE.min(strip_rect.width().max(0.0));
  let button_rect = Rect::from_center_size(
    Pos2::new(strip_rect.max.x - button_size * 0.5, strip_rect.center().y),
    Vec2::splat(button_size),
  );
  let tabs_viewport_max_x = (button_rect.min.x - 8.0).max(strip_rect.min.x);
  let tabs_rect = Rect::from_min_max(strip_rect.min, Pos2::new(tabs_viewport_max_x, strip_rect.max.y));
  let tabs_viewport_width = tabs_rect.width().max(0.0);

  let tab_count = app.tabs.len();
  let can_close_tabs = tab_count > 1;
  let sizing = compute_tab_strip_sizing(tabs_viewport_width, tab_count);

  #[cfg(test)]
  let mut tab_rects_for_test: Vec<Rect> = Vec::new();

  let mut active_tab_rect: Option<Rect> = None;
  let mut scroll_offset_x: f32 = 0.0;

  if tabs_viewport_width > 0.0 {
    let mut tabs_ui = ui.child_ui(tabs_rect, egui::Layout::left_to_right(egui::Align::Center));
    tabs_ui.set_clip_rect(tabs_rect);
    let scroll_output = egui::ScrollArea::horizontal()
      .id_source("tab_strip_scroll")
      .auto_shrink([false, true])
      .show(&mut tabs_ui, |ui| {
        ui.spacing_mut().item_spacing = Vec2::new(TAB_GAP, 0.0);
        ui.horizontal(|ui| {
          for tab in &app.tabs {
            let is_active = app.active_tab_id() == Some(tab.id);
            let favicon_tex = favicon_for_tab(tab.id);
            let (tab_rect, maybe_action) = tab_ui(
              ui,
              motion,
              tab,
              is_active,
              can_close_tabs,
              sizing.tab_width,
              favicon_tex,
            );
            if is_active {
              active_tab_rect = Some(tab_rect);
            }
            #[cfg(test)]
            tab_rects_for_test.push(tab_rect);

            if let Some(action) = maybe_action {
              actions.push(action);
            }
          }
        });
      });
    scroll_offset_x = scroll_output.state.offset.x;
  }

  // Micro-interaction: animate the active tab underline position/width.
  if let Some(active_rect) = active_tab_rect {
    let underline_id = ui.make_persistent_id("tab_strip_active_underline");
    // Animate in scroll-content coordinates so the underline tracks scrolling without lag.
    let target_center_content_x = active_rect.center().x - tabs_rect.min.x + scroll_offset_x;
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

    let center_screen_x = tabs_rect.min.x - scroll_offset_x + center_content_x;
    let x0 = center_screen_x - width * 0.5;
    let x1 = center_screen_x + width * 0.5;
    let y = active_rect.max.y - ACTIVE_UNDERLINE_HEIGHT * 0.5;
    ui
      .painter()
      .with_clip_rect(tabs_rect)
      .line_segment(
        [Pos2::new(x0, y), Pos2::new(x1, y)],
        Stroke::new(ACTIVE_UNDERLINE_HEIGHT, ui.visuals().selection.stroke.color),
      );
  }

  // New tab button stays visible even when the tab list overflows.
  let new_tab_resp = ui
    .put(button_rect, egui::Button::new("+"))
    .on_hover_text("New tab (Ctrl/Cmd+T)");
  new_tab_resp.widget_info(|| {
    egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y::ChromeIconButton::NewTab.label())
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
}
