#![cfg(feature = "browser_ui")]

//! History side panel UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (navigation, persistence, worker messages) are performed by the
//! caller (typically `src/bin/browser.rs`).

use super::{
  a11y_labels, icon_button, panel_empty_state, panel_header_with_actions, panel_list_row,
  panel_search_field, BrowserIcon, GlobalHistoryEntry, GlobalHistoryStore,
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

  egui::SidePanel::right("fastr_history_panel")
    .resizable(true)
    .default_width(360.0)
    .show(ctx, |ui| {
      // -------------------------------------------------------------------
      // Header
      // -------------------------------------------------------------------
      panel_header_with_actions(
        ui,
        BrowserIcon::History,
        "History",
        |ui| {
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
        },
        || {
          out.close_requested = true;
        },
      );

      ui.add_space(6.0);

      // -------------------------------------------------------------------
      // Search
      // -------------------------------------------------------------------
      let search_out = panel_search_field(
        ui,
        "history_panel_search",
        search_text,
        "Search history…",
        request_focus_search,
        "Search history",
      );
      if search_out.focus_requested
        || search_out.response.has_focus()
        || search_out.response.clicked()
        || search_out
          .clear_response
          .as_ref()
          .is_some_and(|resp| resp.clicked())
      {
        out.unfocus_page = true;
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
        let (headline, detail, icon, action_label) = if history.entries.is_empty() {
          (
            "No history yet",
            Some("Pages you visit will appear here."),
            BrowserIcon::History,
            None,
          )
        } else {
          (
            "No results",
            Some("Try a different search query."),
            BrowserIcon::Search,
            (!query_is_empty).then_some("Clear search"),
          )
        };

        let empty_out = panel_empty_state(ui, icon, headline, detail, action_label);
        if let Some(action_resp) = empty_out.action_response {
          action_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear history search")
          });
          if action_resp.clicked() {
            search_text.clear();
            *request_focus_search = true;
            out.unfocus_page = true;
          }
        }
        return;
      }

      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
          ui.spacing_mut().item_spacing.y = 6.0;
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
            let mut action_clicked = false;
            let row_resp = panel_list_row(
              ui,
              ("history_row", idx),
              egui::RichText::new(title).strong(),
              Some(
                egui::RichText::new(url)
                  .small()
                  .color(ui.visuals().weak_text_color())
                  .into(),
              ),
              Some(
                egui::RichText::new(ts.as_str())
                  .small()
                  .color(ui.visuals().weak_text_color())
                  .into(),
              ),
              None,
              |ui| {
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
              },
            );

            row_resp.response.widget_info({
              let label = a11y_labels::history_open_label(Some(&entry_label), url.as_str());
              move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
            });

            if row_resp.response.clicked() && !action_clicked {
              out.open_url = Some(url.clone());
            }
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
