use crate::ui::browser_app::{BrowserAppState, BrowserTabState};
use crate::ui::messages::TabId;
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
  let stroke = visuals.widgets.inactive.fg_stroke;
  painter.rect_filled(rect, 3.0, fill);
  painter.rect_stroke(rect, 3.0, stroke);
}

fn tab_ui(
  ui: &mut egui::Ui,
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

  let visuals = ui.style().visuals.clone();

  let bg = if is_active {
    visuals.widgets.active.bg_fill
  } else if response.hovered() {
    visuals.widgets.hovered.bg_fill
  } else {
    visuals.widgets.inactive.bg_fill
  };
  let rounding = 8.0;
  {
    let painter = ui.painter();
    painter.rect_filled(tab_rect, rounding, bg);

    if is_active {
      let y = tab_rect.max.y - ACTIVE_UNDERLINE_HEIGHT * 0.5;
      painter.line_segment(
        [
          Pos2::new(tab_rect.min.x + 10.0, y),
          Pos2::new(tab_rect.max.x - 10.0, y),
        ],
        Stroke::new(ACTIVE_UNDERLINE_HEIGHT, visuals.widgets.active.fg_stroke.color),
      );
    }
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
    close_clicked = close_resp.clicked();

    if close_resp.hovered() {
      ui.painter().rect_filled(
        close_rect,
        6.0,
        visuals.widgets.hovered.bg_fill.gamma_multiply(0.85),
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
    let label = egui::Label::new(egui::RichText::new(title).strong())
      .truncate(true)
      .wrap(false);
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

  let mut tab_rects_for_test: Vec<Rect> = Vec::new();

  if tabs_viewport_width > 0.0 {
    let mut tabs_ui = ui.child_ui(tabs_rect, egui::Layout::left_to_right(egui::Align::Center));
    tabs_ui.set_clip_rect(tabs_rect);
    egui::ScrollArea::horizontal()
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
              tab,
              is_active,
              can_close_tabs,
              sizing.tab_width,
              favicon_tex,
            );
            tab_rects_for_test.push(tab_rect);

            if let Some(action) = maybe_action {
              actions.push(action);
            }
          }
        });
      });
  }

  // New tab button stays visible even when the tab list overflows.
  let new_tab_resp =
    ui.put(button_rect, egui::Button::new("+")).on_hover_text("New tab (Ctrl/Cmd+T)");
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
