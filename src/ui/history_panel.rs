#![cfg(feature = "browser_ui")]

//! History side panel UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (navigation, persistence, worker messages) are performed by the
//! caller (typically `src/bin/browser.rs`).

use super::{
  a11y_labels, icon_button, icon_tinted, motion::UiMotion, BrowserIcon, GlobalHistoryEntry,
  GlobalHistoryStore,
};

#[derive(Debug, Default)]
pub struct HistoryPanelOutput {
  pub close_requested: bool,
  pub unfocus_page: bool,
  pub open_url: Option<String>,
  pub open_in_new_tab: Option<String>,
  pub delete_index: Option<usize>,
  pub open_clear_browsing_data_dialog: bool,
}

pub fn history_panel_ui(
  ctx: &egui::Context,
  history: &GlobalHistoryStore,
  search_text: &mut String,
  request_focus_search: &mut bool,
) -> HistoryPanelOutput {
  let mut out = HistoryPanelOutput::default();

  // Simple lerp helpers for subtle hover transitions (honors reduced motion via UiMotion).
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
  fn lerp_stroke(a: egui::Stroke, b: egui::Stroke, t: f32) -> egui::Stroke {
    egui::Stroke::new(lerp(a.width, b.width, t), lerp_color(a.color, b.color, t))
  }

  egui::SidePanel::right("fastr_history_panel")
    .resizable(true)
    .default_width(360.0)
    .show(ctx, |ui| {
      let motion = UiMotion::from_ctx(ui.ctx());

      // -------------------------------------------------------------------
      // Header
      // -------------------------------------------------------------------
      ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        icon_tinted(
          ui,
          BrowserIcon::History,
          ui.spacing().icon_width,
          ui.visuals().text_color(),
        );
        ui.heading("History");

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          let close_resp = icon_button(ui, BrowserIcon::Close, "Close (Esc)", true);
          close_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close history panel")
          });
          if close_resp.clicked() {
            out.close_requested = true;
          }

          let clear_resp = ui.add(
            egui::Button::new(
              egui::RichText::new("Clear browsing data…")
                .small()
                .color(ui.visuals().hyperlink_color),
            )
            .frame(false),
          );
          clear_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear browsing data")
          });
          let clear_resp = clear_resp.on_hover_text("Clear browsing data…");
          if clear_resp.clicked() {
            out.open_clear_browsing_data_dialog = true;
          }
        });
      });

      ui.add_space(6.0);

      // -------------------------------------------------------------------
      // Search pill
      // -------------------------------------------------------------------
      let search_id = ui.make_persistent_id("history_panel_search");
      let mut search_has_focus = false;
      let pill_rounding = egui::Rounding::same(999.0);
      let pill_margin = egui::Margin::symmetric(
        ui.spacing().button_padding.x,
        ui.spacing().button_padding.y * 0.6,
      );
      let pill_inner = egui::Frame::none()
        .fill(ui.visuals().widgets.inactive.bg_fill)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .rounding(pill_rounding)
        .inner_margin(pill_margin)
        .show(ui, |ui| {
          ui.set_width(ui.available_width());
          ui.horizontal(|ui| {
            icon_tinted(
              ui,
              BrowserIcon::Search,
              16.0,
              ui.visuals().weak_text_color(),
            );

            let search = ui.add(
              egui::TextEdit::singleline(search_text)
                .id(search_id)
                .hint_text("Search history…")
                .desired_width(f32::INFINITY)
                .frame(false),
            );
            search.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Search history")
            });
            if *request_focus_search {
              search.request_focus();
              *request_focus_search = false;
              out.unfocus_page = true;
            }
            if search.has_focus() || search.clicked() {
              out.unfocus_page = true;
            }
            search_has_focus = search.has_focus();
          });
        });

      // Custom focus ring for the pill when the embedded TextEdit has focus.
      if search_has_focus {
        let focus_stroke = ui.visuals().selection.stroke;
        let expand = 1.0 + focus_stroke.width * 0.5;
        let rect = pill_inner.response.rect.expand(expand);
        let rounding = egui::Rounding::same(pill_rounding.nw + expand);
        ui.painter().rect_stroke(rect, rounding, focus_stroke);
      }

      ui.add_space(8.0);
      ui.separator();
      ui.add_space(4.0);

      // -------------------------------------------------------------------
      // Results list
      // -------------------------------------------------------------------
      const HISTORY_PANEL_LIMIT: usize = 500;
      // Avoid holding an `&str` borrow into `search_text` across UI closures, since some empty
      // states mutate `search_text` (e.g. "Clear search").
      let query = search_text.trim().to_string();
      let query_is_empty = query.is_empty();
      let results: Vec<(usize, &GlobalHistoryEntry)> = if query_is_empty {
        history.iter_recent().take(HISTORY_PANEL_LIMIT).collect()
      } else {
        history.search(&query, HISTORY_PANEL_LIMIT)
      };

      if results.is_empty() {
        ui.add_space(32.0);
        ui.vertical_centered(|ui| {
          ui.add_space(6.0);

          let (title, hint, icon) = if history.entries.is_empty() {
            (
              "No history yet",
              "Pages you visit will appear here.",
              BrowserIcon::History,
            )
          } else {
            (
              "No results",
              "Try a different search query.",
              BrowserIcon::Search,
            )
          };

          icon_tinted(ui, icon, 34.0, ui.visuals().weak_text_color());
          ui.add_space(10.0);
          ui.label(egui::RichText::new(title).strong());
          ui.label(
            egui::RichText::new(hint)
              .small()
              .color(ui.visuals().weak_text_color()),
          );

          if !history.entries.is_empty() && !query_is_empty {
            ui.add_space(10.0);
            let clear_search = ui.button("Clear search");
            clear_search.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear history search")
            });
            if clear_search.clicked() {
              search_text.clear();
              *request_focus_search = true;
              out.unfocus_page = true;
            }
          }
        });
        return;
      }

      let row_padding = egui::vec2(ui.spacing().button_padding.x, ui.spacing().button_padding.y);
      let title_h = ui.text_style_height(&egui::TextStyle::Body);
      let small_h = ui.text_style_height(&egui::TextStyle::Small);
      let row_h = (row_padding.y * 2.0) + title_h + (small_h * 2.0) + 6.0;
      let rounding = ui.visuals().widgets.inactive.rounding;
      let base_fill = ui.visuals().widgets.inactive.bg_fill;
      let hover_fill = ui.visuals().widgets.hovered.bg_fill;
      let base_stroke = ui.visuals().widgets.inactive.bg_stroke;
      let hover_stroke = ui.visuals().widgets.hovered.bg_stroke;

      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
          for (idx, entry) in results {
            let title = entry
              .title
              .as_deref()
              .map(str::trim)
              .filter(|t| !t.is_empty())
              .unwrap_or(entry.url.as_str());
            let url = &entry.url;
            let entry_label = if title == url.as_str() {
              title.to_string()
            } else {
              format!("{title} ({url})")
            };

            let ts = format_history_timestamp_ms(entry.visited_at_ms)
              .unwrap_or_else(|| "Unknown time".to_string());

            let (_, row_rect) = ui.allocate_space(egui::vec2(ui.available_width(), row_h));
            let row_id = ui.make_persistent_id(("history_row", idx));
            let row_resp = ui.interact(row_rect, row_id, egui::Sense::click());

            let hover_t = motion.animate_bool(
              ui.ctx(),
              row_id.with("hover"),
              row_resp.hovered(),
              motion.durations.hover_fade,
            );
            let fill = lerp_color(base_fill, hover_fill, hover_t);
            let stroke = lerp_stroke(base_stroke, hover_stroke, hover_t);
            ui.painter().rect(row_rect, rounding, fill, stroke);

            if row_resp.has_focus() {
              let focus_stroke = ui.visuals().selection.stroke;
              let expand = 1.0 + focus_stroke.width * 0.5;
              let focus_rect = row_rect.expand(expand);
              let focus_rounding = egui::Rounding::same(rounding.nw + expand);
              ui.painter()
                .rect_stroke(focus_rect, focus_rounding, focus_stroke);
            }

            row_resp.widget_info({
              let label = a11y_labels::history_open_label(Some(&entry_label), url.as_str());
              move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
            });

            let mut action_clicked = false;
            ui.allocate_ui_at_rect(row_rect.shrink2(row_padding), |ui| {
              ui.spacing_mut().item_spacing.x = 6.0;

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                  let delete_resp = icon_button(ui, BrowserIcon::Trash, "Delete", true);
                  delete_resp.widget_info({
                    let label = a11y_labels::history_delete_label(Some(&entry_label), url.as_str());
                    move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
                  });
                  if delete_resp.clicked() {
                    out.delete_index = Some(idx);
                    action_clicked = true;
                  }

                let new_tab_resp =
                  icon_button(ui, BrowserIcon::OpenInNewTab, "Open in new tab", true);
                new_tab_resp.widget_info({
                  let label =
                    a11y_labels::history_open_in_new_tab_label(Some(&entry_label), url.as_str());
                  move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
                });
                if new_tab_resp.clicked() {
                  out.open_in_new_tab = Some(url.clone());
                  action_clicked = true;
                }

                let open_resp = ui.small_button("Open");
                open_resp.widget_info({
                  let label = a11y_labels::history_open_label(Some(&entry_label), url.as_str());
                  move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
                });
                if open_resp.clicked() {
                  out.open_url = Some(url.clone());
                  action_clicked = true;
                }

                // Main text block (fills remaining width).
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                  ui.set_width(ui.available_width());
                  ui.add(
                    egui::Label::new(egui::RichText::new(title).strong())
                      .wrap(false)
                      .truncate(true),
                  );
                  ui.add(
                    egui::Label::new(
                      egui::RichText::new(url)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    )
                    .wrap(false)
                    .truncate(true),
                  );
                  ui.add(
                    egui::Label::new(
                      egui::RichText::new(ts)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    )
                    .wrap(false)
                    .truncate(true),
                  );
                });
              });
            });

            if row_resp.clicked() && !action_clicked {
              out.open_url = Some(url.clone());
            }

            ui.add_space(6.0);
          }
        });
    });

  out
}

fn format_history_timestamp_ms(visited_at_ms: u64) -> Option<String> {
  use chrono::{DateTime, Local, Utc};
  use std::time::{Duration, UNIX_EPOCH};

  if visited_at_ms == 0 {
    return None;
  }
  let time = UNIX_EPOCH.checked_add(Duration::from_millis(visited_at_ms))?;
  let utc: DateTime<Utc> = time.into();
  Some(
    utc
      .with_timezone(&Local)
      .format("%Y-%m-%d %H:%M")
      .to_string(),
  )
}
