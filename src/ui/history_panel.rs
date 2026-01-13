#![cfg(feature = "browser_ui")]

//! History side panel UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (navigation, persistence, worker messages) are performed by the
//! caller (typically `src/bin/browser.rs`).

use super::{
  a11y, a11y_labels, history_timestamp, icon_button, panel_empty_state, panel_header_with_actions,
  panel_list_row, panel_search_field, BrowserIcon, GlobalHistorySearcher, GlobalHistoryStore,
};

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Arc;

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
  searcher: &mut GlobalHistorySearcher,
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
        a11y::HISTORY_PANEL_SEARCH_LABEL,
      );
      if search_out.request_close {
        out.close_requested = true;
      }
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
      let mut query_is_empty = false;
      let results = {
        let query = search_text.trim();
        query_is_empty = query.is_empty();
        searcher.search_indices(history, query, HISTORY_PANEL_LIMIT)
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

      // Virtualize the rows so stable queries do not require walking the entire (up to 500 row)
      // result set every frame; only visible rows are built.
      let row_gap = 6.0;
      let row_height = {
        let padding = ui.spacing().button_padding;
        let text_h = ui.text_style_height(&egui::TextStyle::Body);
        let small_h = ui.text_style_height(&egui::TextStyle::Small);
        let content_h = text_h + small_h + small_h;
        (content_h + padding.y * 2.0).max(ui.spacing().interact_size.y.max(30.0))
      };
      let row_total_h = row_height + row_gap;

      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_total_h, results.len(), |ui, row_range| {
          // We account for spacing via `row_gap`, so avoid implicit spacing from egui layouts.
          ui.spacing_mut().item_spacing.y = 0.0;

          for row_idx in row_range {
            let Some(&idx) = results.get(row_idx) else {
              continue;
            };

            let entry = &history.entries[idx];
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

            let ts = format_history_timestamp_ms_cached(ctx, entry.visited_at_ms);
            let ts_text: egui::WidgetText = match ts.as_deref() {
              Some(ts) => ts.into(),
              None => "Unknown time".into(),
            };
            let mut action_clicked = false;
            let row_resp = panel_list_row(
              ui,
              ("history_row", url.as_str()),
              egui::RichText::new(title).strong(),
              Some(url.as_str().into()),
              Some(ts_text),
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

            // Manual spacing so `show_rows` can assume a constant row height.
            ui.add_space(row_gap);
          }
        });
    });

  out
}

// -----------------------------------------------------------------------------
// Cached timestamp formatting
// -----------------------------------------------------------------------------

/// Cache capacity for formatted history timestamps.
///
/// The History panel can display up to 500 rows, but keeping a larger cache avoids churn when users
/// scroll/search and when the same browser session keeps accumulating history.
const HISTORY_TIMESTAMP_CACHE_CAPACITY: usize = 4_096;

#[derive(Debug, Clone)]
struct HistoryPanelCache {
  // Keyed by unix-epoch minute; the UI output format does not include seconds, so all instants
  // within the same minute map to the same display string.
  timestamps_by_minute: LruCache<u64, Arc<str>>,
}

impl Default for HistoryPanelCache {
  fn default() -> Self {
    Self {
      timestamps_by_minute: LruCache::new(
        NonZeroUsize::new(HISTORY_TIMESTAMP_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN),
      ),
    }
  }
}

fn history_panel_cache_id() -> egui::Id {
  egui::Id::new("fastr_history_panel_cache")
}

fn lookup_cached_timestamp(ctx: &egui::Context, minute_key: u64) -> Option<Arc<str>> {
  ctx.data_mut(|d| {
    let cache = d.get_temp_mut_or_default::<HistoryPanelCache>(history_panel_cache_id());
    cache.timestamps_by_minute.get(&minute_key).cloned()
  })
}

fn insert_cached_timestamp(ctx: &egui::Context, minute_key: u64, value: Arc<str>) {
  ctx.data_mut(|d| {
    let cache = d.get_temp_mut_or_default::<HistoryPanelCache>(history_panel_cache_id());
    cache.timestamps_by_minute.put(minute_key, value);
  });
}

fn format_history_timestamp_ms_cached(ctx: &egui::Context, visited_at_ms: u64) -> Option<Arc<str>> {
  if visited_at_ms == 0 {
    return None;
  }

  // Since we only show minutes, use the epoch minute as a stable cache key to maximize hits.
  let minute_key = visited_at_ms / 60_000;
  if let Some(cached) = lookup_cached_timestamp(ctx, minute_key) {
    return Some(cached);
  }

  let formatted = history_timestamp::format_history_timestamp_ms(visited_at_ms)?;
  let arc: Arc<str> = Arc::from(formatted);
  insert_cached_timestamp(ctx, minute_key, arc.clone());
  Some(arc)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::history_panel_ui;
  use crate::ui::{a11y_labels, a11y_test_util, GlobalHistoryStore};

  fn begin_frame_with_events(ctx: &egui::Context, events: Vec<egui::Event>) {
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

  fn begin_frame(ctx: &egui::Context) {
    begin_frame_with_events(ctx, Vec::new());
  }

  fn key_press(key: egui::Key) -> egui::Event {
    egui::Event::Key {
      key,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers::default(),
    }
  }

  fn accesskit_button_id_by_name(output: &egui::FullOutput, name: &str) -> String {
    let snapshot = a11y_test_util::accesskit_snapshot_from_full_output(output);
    let json = a11y_test_util::accesskit_pretty_json_from_full_output(output);
    let matches: Vec<_> = snapshot
      .nodes
      .iter()
      .filter(|node| node.role == "Button" && node.name == name)
      .collect();

    assert!(
      matches.len() == 1,
      "expected exactly one AccessKit Button named {name:?}; found {}.\n\nsnapshot:\n{json}",
      matches.len()
    );

    matches[0].id.clone()
  }

  #[test]
  fn history_panel_accesskit_node_ids_stable_across_reorder() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    let url_a = "https://example.com/a".to_string();
    let url_b = "https://example.com/b".to_string();
    history.record(url_a.clone(), None);
    history.record(url_b.clone(), None);

    // The History panel uses the URL as the context string when the title is missing.
    let stored_url_b = history
      .entries
      .last()
      .expect("history must contain at least two entries")
      .url
      .clone();
    let delete_label = a11y_labels::history_delete_label(Some(&stored_url_b), &stored_url_b);

    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In headless unit tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();

    let mut search_text = String::new();
    let mut request_focus_search = false;

    begin_frame(&ctx);
    let _out = history_panel_ui(&ctx, &history, &mut search_text, &mut request_focus_search);
    let output1 = ctx.end_frame();
    let id1 = accesskit_button_id_by_name(&output1, &delete_label);

    // Reorder the history store by recording another visit to a different URL. This will move the
    // corresponding entry to the most-recent position and shift backing-store indices.
    history.record(url_a, None);

    begin_frame(&ctx);
    let _out = history_panel_ui(&ctx, &history, &mut search_text, &mut request_focus_search);
    let output2 = ctx.end_frame();
    let id2 = accesskit_button_id_by_name(&output2, &delete_label);

    let snapshot1 = a11y_test_util::accesskit_pretty_json_from_full_output(&output1);
    let snapshot2 = a11y_test_util::accesskit_pretty_json_from_full_output(&output2);
    assert_eq!(
      id1, id2,
      "expected AccessKit node id for {delete_label:?} to remain stable across history reorder.\n\nbefore:\n{snapshot1}\n\nafter:\n{snapshot2}"
    );
  }

  #[test]
  fn escape_clears_search_then_requests_close() {
    let ctx = egui::Context::default();
    let history = GlobalHistoryStore::default();

    let mut search_text = String::new();
    let mut request_focus_search = true;

    // Frame 1: open panel and focus the search field.
    begin_frame_with_events(&ctx, Vec::new());
    let out = history_panel_ui(&ctx, &history, &mut search_text, &mut request_focus_search);
    let _ = ctx.end_frame();
    assert!(
      !out.close_requested,
      "focusing the search field should not request closing the panel"
    );

    // Frame 2: with a non-empty query, Escape clears the search but keeps the panel open.
    search_text = "example".to_string();
    begin_frame_with_events(&ctx, vec![key_press(egui::Key::Escape)]);
    let out = history_panel_ui(&ctx, &history, &mut search_text, &mut request_focus_search);
    let _ = ctx.end_frame();
    assert_eq!(search_text, "");
    assert!(
      !out.close_requested,
      "Escape should clear a non-empty query before closing the panel"
    );

    // Frame 3: with an empty query, Escape requests panel close.
    begin_frame_with_events(&ctx, vec![key_press(egui::Key::Escape)]);
    let out = history_panel_ui(&ctx, &history, &mut search_text, &mut request_focus_search);
    let _ = ctx.end_frame();
    assert_eq!(search_text, "");
    assert!(
      out.close_requested,
      "Escape with an empty query should request closing the panel"
    );
  }
}
