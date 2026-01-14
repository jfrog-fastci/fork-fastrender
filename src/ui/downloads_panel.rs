#![cfg(feature = "browser_ui")]

//! Downloads side panel UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (worker messages, OS open/reveal) are performed by the caller
//! (typically `src/bin/browser.rs`).

use smallvec::SmallVec;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::{
  a11y_labels, motion::UiMotion, panel_empty_state, panel_header_with_actions, panel_search_field,
  theme::BrowserTheme, BrowserIcon, DownloadEntry, DownloadId, DownloadStatus, TabId,
};
use super::string_match::contains_ascii_case_insensitive;

fn format_bytes(bytes: u64) -> String {
  const KB: f64 = 1024.0;
  const MB: f64 = KB * 1024.0;
  const GB: f64 = MB * 1024.0;

  let b = bytes as f64;
  if b >= GB {
    format!("{:.1} GiB", b / GB)
  } else if b >= MB {
    format!("{:.1} MiB", b / MB)
  } else if b >= KB {
    format!("{:.1} KiB", b / KB)
  } else {
    format!("{bytes} B")
  }
}

fn download_progress_a11y_label(
  file_name: &str,
  received_bytes: u64,
  total_bytes: Option<u64>,
) -> String {
  let file_name = file_name.trim();
  match total_bytes.filter(|t| *t > 0) {
    Some(total) => {
      if file_name.is_empty() {
        format!(
          "Downloading: {} of {}",
          format_bytes(received_bytes),
          format_bytes(total)
        )
      } else {
        format!(
          "Downloading {file_name}: {} of {}",
          format_bytes(received_bytes),
          format_bytes(total)
        )
      }
    }
    None => {
      if received_bytes > 0 {
        if file_name.is_empty() {
          format!("Downloading: {}", format_bytes(received_bytes))
        } else {
          format!("Downloading {file_name}: {}", format_bytes(received_bytes))
        }
      } else if file_name.is_empty() {
        "Downloading".to_string()
      } else {
        format!("Downloading {file_name}")
      }
    }
  }
}

fn download_status_search_haystack(status: &DownloadStatus) -> &'static str {
  match status {
    DownloadStatus::InProgress { .. } => "downloading inprogress in-progress",
    DownloadStatus::Completed => "completed complete done",
    DownloadStatus::Cancelled => "cancelled canceled",
    DownloadStatus::Failed { .. } => "failed error",
  }
}

fn download_matches_tokens(entry: &DownloadEntry, tokens_lower: &[&str]) -> bool {
  if tokens_lower.is_empty() {
    return true;
  }

  let file_name = entry.file_name.trim();
  let url = entry.url.trim();
  let status = download_status_search_haystack(&entry.status);
  let error = match &entry.status {
    DownloadStatus::Failed { error } => Some(error.trim()).filter(|e| !e.is_empty()),
    _ => None,
  };

  for token_lower in tokens_lower {
    if contains_ascii_case_insensitive(file_name, token_lower)
      || contains_ascii_case_insensitive(url, token_lower)
      || contains_ascii_case_insensitive(status, token_lower)
      || error.is_some_and(|err| contains_ascii_case_insensitive(err, token_lower))
    {
      continue;
    }
    if contains_ascii_case_insensitive(entry.path_display.as_str(), token_lower) {
      continue;
    }
    return false;
  }

  true
}

/// Returns `true` when `entry` should be included for `query`.
///
/// Search semantics:
/// - Query is split by whitespace into tokens; every token must match at least one download field.
/// - Matching is ASCII case-insensitive (non-ASCII bytes must match exactly).
/// - Tokens match against file name, URL, local path display, and status words ("failed", etc).
pub fn download_matches_query(entry: &DownloadEntry, query: &str) -> bool {
  let query = query.trim();
  if query.is_empty() {
    return true;
  }

  // Most queries are already lowercase; avoid allocating unless needed.
  let query_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
    Cow::Owned(query.to_ascii_lowercase())
  } else {
    Cow::Borrowed(query)
  };
  let tokens: SmallVec<[&str; 4]> = query_lower.split_whitespace().collect();
  download_matches_tokens(entry, tokens.as_slice())
}

#[derive(Debug, Default)]
pub struct DownloadsPanelOutput {
  pub close_requested: bool,
  pub clear_completed_requested: bool,
  /// When true, the caller should open a native folder picker and update the configured download
  /// directory.
  pub request_pick_download_dir: bool,
  pub cancel_requests: Vec<(TabId, DownloadId)>,
  pub retry_requests: Vec<(TabId, String, Option<String>)>,
  pub open_requests: Vec<PathBuf>,
  pub reveal_requests: Vec<PathBuf>,
  pub copy_requests: Vec<String>,
}

#[cfg(test)]
fn store_test_id(ctx: &egui::Context, key: impl std::hash::Hash, id: egui::Id) {
  let key = egui::Id::new(key);
  ctx.data_mut(|d| {
    d.insert_temp(key, id);
  });
}

pub fn downloads_panel_ui(
  ctx: &egui::Context,
  downloads: &[DownloadEntry],
  search_query: &mut String,
  theme: &BrowserTheme,
  request_initial_focus: bool,
  download_dir: &Path,
) -> DownloadsPanelOutput {
  let mut out = DownloadsPanelOutput::default();

  fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
      .round()
      .clamp(0.0, 255.0) as u8
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
    egui::Stroke::new(
      a.width + (b.width - a.width) * t,
      lerp_color(a.color, b.color, t),
    )
  }

  fn with_scaled_alpha(color: egui::Color32, alpha_mul: f32) -> egui::Color32 {
    let [r, g, b, a] = color.to_array();
    let a = (a as f32 * alpha_mul).round().clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
  }

  let motion = UiMotion::from_ctx(ctx);

  let has_completed_downloads = downloads
    .iter()
    .any(|entry| matches!(&entry.status, DownloadStatus::Completed));

  egui::SidePanel::right("downloads_panel")
    .resizable(true)
    .default_width(360.0)
    .show(ctx, |ui| {
      let header_out = panel_header_with_actions(
        ui,
        BrowserIcon::Download,
        "Downloads",
        |ui| {
          let change_folder = ui
            .small_button("Change download folder…")
            .on_hover_ui(|ui| {
              ui.label(format!("Current folder: {}", download_dir.display()));
            });
          #[cfg(test)]
          store_test_id(
            ui.ctx(),
            "downloads_panel_change_folder_button_id",
            change_folder.id,
          );
          change_folder.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Change download folder")
          });
          if change_folder.clicked() {
            out.request_pick_download_dir = true;
          }

          let clear_button = egui::Button::new(egui::RichText::new("Clear completed").small());
          let clear_resp = ui.add_enabled(has_completed_downloads, clear_button);
          clear_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear completed downloads")
          });
          #[cfg(test)]
          store_test_id(
            ui.ctx(),
            "downloads_panel_clear_completed_button_id",
            clear_resp.id,
          );
          if clear_resp.clicked() {
            out.clear_completed_requested = true;
          }

          let show_folder = ui.small_button("Show downloads folder");
          #[cfg(test)]
          store_test_id(ui.ctx(), "downloads_panel_show_folder_button_id", show_folder.id);
          show_folder.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Show downloads folder")
          });
          if show_folder.clicked() {
            out.open_requests.push(download_dir.to_path_buf());
          }
        },
        || {
          out.close_requested = true;
        },
      );
      header_out.close_response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close downloads panel")
      });
      ui.add_space(6.0);

      let mut request_focus_search = request_initial_focus;
      let search_out = panel_search_field(
        ui,
        "downloads_panel_search",
        search_query,
        "Search downloads…",
        &mut request_focus_search,
        "Search downloads",
      );
      if search_out.request_close {
        out.close_requested = true;
      }
      #[cfg(test)]
      store_test_id(ui.ctx(), "downloads_panel_search_input_id", search_out.response.id);

      ui.add_space(8.0);
      ui.separator();
      ui.add_space(4.0);

      if downloads.is_empty() {
        panel_empty_state(ui, BrowserIcon::Download, "No downloads yet", None, None);
        return;
      }

      let visuals = ui.visuals().clone();
      let row_rounding = egui::Rounding::same(theme.sizing.corner_radius);
      let row_padding = theme.sizing.padding * 0.75;
      let row_gap = theme.sizing.padding * 0.75;
      let hover_overlay = if visuals.dark_mode {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24)
      } else {
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 14)
      };

      let body_h = ui.text_style_height(&egui::TextStyle::Body);
      let small_h = ui.text_style_height(&egui::TextStyle::Small);
      // Conservatively estimate the progress bar height so rows look consistent even if egui's
      // internal widget sizing changes slightly between versions.
      let progress_h = (ui.spacing().interact_size.y * 0.42).clamp(8.0, 12.0);
      let line_gap = (theme.sizing.padding * 0.25).clamp(2.0, 4.0);

      // Fixed row height for virtualization.
      //
      // Rows can show an extra line (either a progress bar for in-progress downloads, or an error
      // label for failures). We reserve enough space for that extra line on every row so we can use
      // `show_rows` (which expects constant heights).
      let base_content_h = body_h + line_gap + small_h + line_gap + small_h;
      let extra_line_h = line_gap + small_h.max(progress_h);
      let content_h = base_content_h + extra_line_h;
      let row_content_h = (content_h + row_padding * 2.0).ceil();
      let row_total_h = row_content_h + row_gap;

      let query = search_query.trim();
      // Most queries are already lowercase; avoid allocating unless needed.
      let query_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(query.to_ascii_lowercase())
      } else {
        Cow::Borrowed(query)
      };
      let tokens: SmallVec<[&str; 4]> = query_lower.split_whitespace().collect();
      let has_query = !tokens.is_empty();

      let filtered_count = if !has_query {
        None
      } else {
        let matches = downloads
          .iter()
          .rev()
          .filter(|entry| download_matches_tokens(entry, &tokens))
          .count();
        if matches == 0 {
          panel_empty_state(ui, BrowserIcon::Search, "No matching downloads", None, None);
          return;
        }
        Some(matches)
      };
      let row_count = filtered_count.unwrap_or(downloads.len());

      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_total_h, row_count, |ui, row_range| {
          // `show_rows` virtualizes the list: egui only builds the rows that intersect the current
          // viewport. This keeps the downloads panel cost proportional to the number of visible
          // rows, even when the history contains thousands of entries.
          //
          // Important: when virtualizing, per-row ids must be stable so state (hover animations,
          // button interactions, etc.) doesn't get reused for different entries as the viewport
          // changes. We therefore `push_id` per download row.

          // `show_rows` assumes each row consumes exactly `row_total_h` vertical space; don't apply
          // implicit item spacing on top of our own `row_gap` padding.
          ui.spacing_mut().item_spacing.y = 0.0;

          // Fetch the pointer position once per frame; avoid per-row `ctx.input` calls.
          let pointer_pos = ui.ctx().input(|i| i.pointer.hover_pos());

          let mut render_row = |ui: &mut egui::Ui, entry: &DownloadEntry| {
            let width = ui.available_width().max(0.0);
            let (_alloc_id, row_rect) = ui.allocate_space(egui::vec2(width, row_total_h));
            let rect = egui::Rect::from_min_max(
              row_rect.min,
              egui::pos2(
                row_rect.max.x,
                (row_rect.max.y - row_gap).max(row_rect.min.y),
              ),
            );

            let row_id = egui::Id::new(("fastr_download_row", entry.download_id.0));
            let response = ui.interact(rect, row_id, egui::Sense::hover());

            // Use egui's interaction state instead of per-row pointer scanning. `contains_pointer`
            // is used instead of `hovered()` so the row highlight still shows while hovering over
            // child buttons.
            let contains_pointer = pointer_pos.is_some_and(|pos| rect.contains(pos));
            let hover_t = motion.animate_bool(
              ui.ctx(),
              row_id.with("hover"),
              ui.is_enabled() && (response.hovered() || contains_pointer),
              motion.durations.hover_fade,
            );

            let base_fill = visuals.widgets.inactive.bg_fill;
            let base_stroke = visuals.widgets.noninteractive.bg_stroke;
            let hover_stroke = visuals.widgets.hovered.bg_stroke;

            if ui.is_rect_visible(rect) {
              ui.painter().rect_filled(rect, row_rounding, base_fill);
              if hover_t > 0.0 {
                ui.painter().rect_filled(
                  rect,
                  row_rounding,
                  with_scaled_alpha(hover_overlay, hover_t),
                );
              }
              ui.painter().rect_stroke(
                rect,
                row_rounding,
                lerp_stroke(base_stroke, hover_stroke, hover_t),
              );
            }

            let inner_rect = rect.shrink(row_padding);
            ui.allocate_ui_at_rect(inner_rect, |ui| {
              ui.push_id(entry.download_id.0, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, line_gap);
                ui.set_min_width(inner_rect.width());

                ui.add(
                  egui::Label::new(egui::RichText::new(&entry.file_name).strong())
                    .wrap(false)
                    .truncate(true),
                );

                ui.add(
                  egui::Label::new(
                    egui::RichText::new(entry.path_display.as_str())
                      .small()
                      .color(ui.visuals().weak_text_color()),
                  )
                  .wrap(false)
                  .truncate(true),
                );

                let (status_text, status_color, show_progress) = match &entry.status {
                  DownloadStatus::InProgress {
                    received_bytes,
                    total_bytes,
                  } => {
                    let status = if let Some(total) = total_bytes.filter(|t| *t > 0) {
                      format!(
                        "Downloading… {} / {}",
                        format_bytes(*received_bytes),
                        format_bytes(total)
                      )
                    } else {
                      format!("Downloading… {}", format_bytes(*received_bytes))
                    };
                    (status, ui.visuals().weak_text_color(), true)
                  }
                  DownloadStatus::Completed => (
                    "Completed".to_string(),
                    ui.visuals().weak_text_color(),
                    false,
                  ),
                  DownloadStatus::Cancelled => (
                    "Cancelled".to_string(),
                    ui.visuals().weak_text_color(),
                    false,
                  ),
                  DownloadStatus::Failed { .. } => {
                    ("Failed".to_string(), ui.visuals().error_fg_color, false)
                  }
                };

                ui.horizontal(|ui| {
                  ui.add(
                    egui::Label::new(egui::RichText::new(status_text).small().color(status_color))
                      .wrap(false)
                      .truncate(true),
                  );

                  ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| match &entry.status {
                      DownloadStatus::InProgress { .. } => {
                        let cancel_resp = ui.small_button("Cancel");
                        cancel_resp.widget_info(|| {
                          egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            a11y_labels::download_cancel_label(&entry.file_name),
                          )
                        });
                        if cancel_resp.clicked() {
                          out.cancel_requests.push((entry.tab_id, entry.download_id));
                        }
                      }
                      DownloadStatus::Completed => {
                        let reveal_resp = ui.small_button("Show in Folder");
                        reveal_resp.widget_info(|| {
                          egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            a11y_labels::download_show_in_folder_label(&entry.file_name),
                          )
                        });
                        if reveal_resp.clicked() {
                          out.reveal_requests.push(entry.path.clone());
                        }
                        let open_resp = ui.small_button("Open");
                        open_resp.widget_info(|| {
                          egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            a11y_labels::download_open_label(&entry.file_name),
                          )
                        });
                      if open_resp.clicked() {
                        out.open_requests.push(entry.path.clone());
                      }

                      let copy_path_resp = ui.small_button("Copy path");
                      copy_path_resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                          egui::WidgetType::Button,
                          a11y_labels::download_copy_path_label(&entry.file_name),
                        )
                      });
                      if copy_path_resp.clicked() {
                        out.copy_requests.push(entry.path_display.clone());
                      }
                      #[cfg(test)]
                      store_test_id(
                        ui.ctx(),
                        ("downloads_copy_path_button_id", entry.download_id.0),
                        copy_path_resp.id,
                      );

                      let copy_link_resp = ui.small_button("Copy link");
                      copy_link_resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                          egui::WidgetType::Button,
                          a11y_labels::download_copy_link_label(&entry.file_name),
                        )
                      });
                      if copy_link_resp.clicked() {
                        out.copy_requests.push(entry.url.clone());
                      }
                      #[cfg(test)]
                      store_test_id(
                        ui.ctx(),
                        ("downloads_copy_link_button_id", entry.download_id.0),
                        copy_link_resp.id,
                      );
                    }
                    DownloadStatus::Cancelled => {
                      let retry_resp = ui.small_button("Retry");
                      retry_resp.widget_info(|| {
                          egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            a11y_labels::download_retry_label(&entry.file_name),
                          )
                        });
                      if retry_resp.clicked() {
                        out.retry_requests.push(entry.retry_request());
                      }

                      let copy_path_resp = ui.small_button("Copy path");
                      copy_path_resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                          egui::WidgetType::Button,
                          a11y_labels::download_copy_path_label(&entry.file_name),
                        )
                      });
                      if copy_path_resp.clicked() {
                        out.copy_requests.push(entry.path_display.clone());
                      }
                      #[cfg(test)]
                      store_test_id(
                        ui.ctx(),
                        ("downloads_copy_path_button_id", entry.download_id.0),
                        copy_path_resp.id,
                      );

                      let copy_link_resp = ui.small_button("Copy link");
                      copy_link_resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                          egui::WidgetType::Button,
                          a11y_labels::download_copy_link_label(&entry.file_name),
                        )
                      });
                      if copy_link_resp.clicked() {
                        out.copy_requests.push(entry.url.clone());
                      }
                      #[cfg(test)]
                      store_test_id(
                        ui.ctx(),
                        ("downloads_copy_link_button_id", entry.download_id.0),
                        copy_link_resp.id,
                      );
                    }
                    DownloadStatus::Failed { .. } => {
                      let retry_resp = ui.small_button("Retry");
                      retry_resp.widget_info(|| {
                          egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            a11y_labels::download_retry_label(&entry.file_name),
                          )
                        });
                      if retry_resp.clicked() {
                        out.retry_requests.push(entry.retry_request());
                      }

                      let copy_path_resp = ui.small_button("Copy path");
                      copy_path_resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                          egui::WidgetType::Button,
                          a11y_labels::download_copy_path_label(&entry.file_name),
                        )
                      });
                      if copy_path_resp.clicked() {
                        out.copy_requests.push(entry.path_display.clone());
                      }
                      #[cfg(test)]
                      store_test_id(
                        ui.ctx(),
                        ("downloads_copy_path_button_id", entry.download_id.0),
                        copy_path_resp.id,
                      );

                      let copy_link_resp = ui.small_button("Copy link");
                      copy_link_resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                          egui::WidgetType::Button,
                          a11y_labels::download_copy_link_label(&entry.file_name),
                        )
                      });
                      if copy_link_resp.clicked() {
                        out.copy_requests.push(entry.url.clone());
                      }
                      #[cfg(test)]
                      store_test_id(
                        ui.ctx(),
                        ("downloads_copy_link_button_id", entry.download_id.0),
                        copy_link_resp.id,
                      );
                    }
                  },
                );
              });

                if let DownloadStatus::Failed { error } = &entry.status {
                  let err = error.trim();
                  if !err.is_empty() {
                    ui.add(
                      egui::Label::new(
                        egui::RichText::new(err)
                          .small()
                          .color(ui.visuals().error_fg_color),
                      )
                      .wrap(false)
                      .truncate(true),
                    )
                    .on_hover_text(err);
                  }
                }

                if show_progress {
                  if let DownloadStatus::InProgress {
                    received_bytes,
                    total_bytes,
                  } = &entry.status
                  {
                    let a11y_label = download_progress_a11y_label(
                      &entry.file_name,
                      *received_bytes,
                      total_bytes.filter(|t| *t > 0),
                    );
                    if let Some(total) = total_bytes.filter(|t| *t > 0) {
                      let frac = (*received_bytes as f32 / total as f32).clamp(0.0, 1.0);
                      let resp = ui.add(
                        egui::ProgressBar::new(frac)
                          .desired_width(f32::INFINITY)
                          .text(""),
                      );
                      resp.widget_info({
                        let label = a11y_label.clone();
                        move || {
                          egui::WidgetInfo::labeled(
                            // `egui` 0.23 does not expose a dedicated progress widget type. Provide
                            // an explicit label so screen readers announce meaningful context.
                            egui::WidgetType::Label,
                            label.clone(),
                          )
                        }
                      });
                    } else {
                      let resp = ui.add(
                        egui::ProgressBar::new(0.0)
                          .desired_width(f32::INFINITY)
                          .animate(motion.enabled)
                          .text(""),
                      );
                      resp.widget_info({
                        let label = a11y_label.clone();
                        move || {
                          egui::WidgetInfo::labeled(
                            // `egui` 0.23 does not expose a dedicated progress widget type. Provide
                            // an explicit label so screen readers announce meaningful context.
                            egui::WidgetType::Label,
                            label.clone(),
                          )
                        }
                      });
                    }
                  }
                }
              });
            });
          };

          if !has_query {
            let total = downloads.len();
            for row_idx in row_range {
              let Some(entry) = total
                .checked_sub(1)
                .and_then(|last| last.checked_sub(row_idx))
                .and_then(|idx| downloads.get(idx))
              else {
                continue;
              };
              render_row(ui, entry);
            }
          } else {
            let mut match_idx = 0usize;
            for entry in downloads.iter().rev() {
              if !download_matches_tokens(entry, &tokens) {
                continue;
              }
              if match_idx < row_range.start {
                match_idx += 1;
                continue;
              }
              if match_idx >= row_range.end {
                break;
              }
              render_row(ui, entry);
              match_idx += 1;
            }
          }
        });
    });

  out
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use std::path::PathBuf;

  use crate::ui::{a11y_labels, a11y_test_util};
  use crate::ui::theme::BrowserTheme;
  use crate::ui::{DownloadEntry, DownloadId, DownloadStatus, TabId};

  use super::{download_matches_query, download_progress_a11y_label, downloads_panel_ui};

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

  fn left_click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    vec![
      egui::Event::PointerMoved(pos),
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
      },
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
      },
    ]
  }

  fn find_text_center(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<egui::Pos2> {
    fn in_shape(shape: &egui::epaint::Shape, needle: &str) -> Option<egui::Pos2> {
      match shape {
        egui::epaint::Shape::Text(text) => text
          .galley
          .text()
          .contains(needle)
          .then_some(text.pos + text.galley.size() / 2.0),
        egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|s| in_shape(s, needle)),
        _ => None,
      }
    }

    shapes.iter().find_map(|clipped| in_shape(&clipped.shape, needle))
  }

  fn expect_temp_id(
    ctx: &egui::Context,
    key: impl std::hash::Hash + std::fmt::Debug + Copy,
  ) -> egui::Id {
    let id_key = egui::Id::new(key);
    ctx
      .data(|d| d.get_temp::<egui::Id>(id_key))
      .unwrap_or_else(|| panic!("expected temp id {key:?}"))
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
  fn download_progress_a11y_label_contains_file_name() {
    let label = download_progress_a11y_label("example.zip", 1_024, Some(2_048));
    assert!(!label.trim().is_empty(), "label should not be empty");
    assert!(
      label.contains("example.zip"),
      "expected label to mention file name; got {label:?}"
    );
  }

  #[test]
  fn show_downloads_folder_uses_injected_download_dir() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::light(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    // Frame 0: capture the show-folder button id.
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    let show_folder_id = expect_temp_id(&ctx, "downloads_panel_show_folder_button_id");

    // Frame 1: move focus to the show-folder button.
    ctx.memory_mut(|mem| mem.request_focus(show_folder_id));
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(show_folder_id)),
      "expected show downloads folder button to have focus"
    );

    // Frame 2: press Enter; should enqueue an open request for the injected download_dir.
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let output = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();

    assert_eq!(
      output.open_requests,
      vec![download_dir.clone()],
      "expected Show downloads folder to open injected dir"
    );
  }

  #[test]
  fn show_downloads_folder_click_emits_open_request() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::light(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    // Frame 0: capture the button location.
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let output = ctx.end_frame();
    let pos = find_text_center(&output.shapes, "Show downloads folder")
      .expect("failed to find Show downloads folder button label in egui shapes");

    // Frame 1: click the button.
    begin_frame(&ctx, left_click_at(pos));
    let output = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();

    assert_eq!(
      output.open_requests,
      vec![download_dir],
      "expected click on Show downloads folder to open injected dir"
    );
  }

  #[test]
  fn downloads_panel_accesskit_node_ids_stable_across_row_insertion() {
    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In headless unit tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();
    let theme = BrowserTheme::dark(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    let download_a = DownloadEntry {
      download_id: DownloadId(1),
      tab_id: TabId(1),
      url: "https://example.com/a.zip".to_string(),
      file_name: "a.zip".to_string(),
      path: PathBuf::from("downloads/a.zip"),
      path_display: "downloads/a.zip".to_string(),
      status: DownloadStatus::Completed,
    };
    let copy_path_label = a11y_labels::download_copy_path_label(&download_a.file_name);

    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[download_a.clone()],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let output1 = ctx.end_frame();
    let id1 = accesskit_button_id_by_name(&output1, &copy_path_label);

    // Insert another download at the end of the slice so it becomes the most recent entry, shifting
    // the existing row down (virtualized list ordering changes).
    let download_b = DownloadEntry {
      download_id: DownloadId(2),
      tab_id: TabId(1),
      url: "https://example.com/b.zip".to_string(),
      file_name: "b.zip".to_string(),
      path: PathBuf::from("downloads/b.zip"),
      path_display: "downloads/b.zip".to_string(),
      status: DownloadStatus::Completed,
    };
    let downloads = vec![download_a.clone(), download_b];

    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let output2 = ctx.end_frame();
    let id2 = accesskit_button_id_by_name(&output2, &copy_path_label);

    let snapshot1 = a11y_test_util::accesskit_pretty_json_from_full_output(&output1);
    let snapshot2 = a11y_test_util::accesskit_pretty_json_from_full_output(&output2);
    assert_eq!(
      id1, id2,
      "expected AccessKit node id for {copy_path_label:?} to remain stable when a download row is inserted above it.\n\nbefore:\n{snapshot1}\n\nafter:\n{snapshot2}"
    );
  }

  #[test]
  fn request_initial_focus_moves_focus_to_search_input() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::light(None);
    let download_dir = PathBuf::from("test-download-dir");
    let downloads: Vec<DownloadEntry> = Vec::new();
    let mut search_query = String::new();

    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      true,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    let search_id = expect_temp_id(&ctx, "downloads_panel_search_input_id");

    assert!(
      ctx.memory(|mem| mem.has_focus(search_id)),
      "expected request_initial_focus=true to move focus to downloads search input"
    );
  }

  #[test]
  fn change_download_folder_button_sets_flag() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::light(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    // Frame 0: capture the change-folder button id.
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    let change_folder_id = expect_temp_id(&ctx, "downloads_panel_change_folder_button_id");

    // Frame 1: move focus to the change-folder button.
    ctx.memory_mut(|mem| mem.request_focus(change_folder_id));
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(change_folder_id)),
      "expected Change download folder button to have focus"
    );

    // Frame 2: press Enter; should set the request_pick_download_dir flag.
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let output = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();

    assert!(
      output.request_pick_download_dir,
      "expected Change download folder to set output flag"
    );
  }

  #[test]
  fn change_download_folder_click_emits_request() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::light(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    // Frame 0: capture the button location.
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let output = ctx.end_frame();
    let pos = find_text_center(&output.shapes, "Change download folder")
      .expect("failed to find Change download folder button label in egui shapes");

    // Frame 1: click the button.
    begin_frame(&ctx, left_click_at(pos));
    let output = downloads_panel_ui(
      &ctx,
      &[],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();

    assert!(
      output.request_pick_download_dir,
      "expected click on Change download folder to set output flag"
    );
  }

  fn sample_entry(file_name: &str, url: &str) -> DownloadEntry {
    DownloadEntry {
      download_id: DownloadId(1),
      tab_id: TabId(1),
      url: url.to_string(),
      file_name: file_name.to_string(),
      path: PathBuf::from("/tmp/example"),
      path_display: "/tmp/example".to_string(),
      status: DownloadStatus::Completed,
      started_at_ms: Some(1),
      finished_at_ms: Some(2),
    }
  }

  #[test]
  fn download_matches_query_empty_query_matches_all() {
    let entry = sample_entry("example.zip", "https://example.com/example.zip");
    assert!(download_matches_query(&entry, ""));
    assert!(download_matches_query(&entry, "   "));
  }

  #[test]
  fn download_matches_query_matches_file_name_case_insensitively() {
    let entry = sample_entry("Example.ZIP", "https://example.com/other");
    assert!(download_matches_query(&entry, "example"));
    assert!(download_matches_query(&entry, "ZIP"));
    assert!(download_matches_query(&entry, "ple.z"));
  }

  #[test]
  fn download_matches_query_matches_url_case_insensitively() {
    let entry = sample_entry("file.bin", "https://EXAMPLE.com/Path/File.BIN");
    assert!(download_matches_query(&entry, "example.com"));
    assert!(download_matches_query(&entry, "path/file"));
    assert!(download_matches_query(&entry, "FILE.bin"));
  }

  #[test]
  fn download_matches_query_rejects_non_matches() {
    let entry = sample_entry("example.zip", "https://example.com/example.zip");
    assert!(!download_matches_query(&entry, "nope"));
  }

  #[test]
  fn download_matches_query_tokenized_matches_filename_url_and_path() {
    let entry = DownloadEntry {
      download_id: DownloadId(1),
      tab_id: TabId(1),
      url: "https://example.com/files/Report.pdf".to_string(),
      file_name: "Report.pdf".to_string(),
      path: PathBuf::from("/home/user/Downloads/Report.pdf"),
      path_display: "/home/user/Downloads/Report.pdf".to_string(),
      status: DownloadStatus::Completed,
    };

    assert!(
      download_matches_query(&entry, "report example downloads"),
      "expected tokens to match across file name/url/path"
    );
    assert!(
      !download_matches_query(&entry, "report example missingtoken"),
      "expected non-matching token to reject the entry"
    );
  }

  #[test]
  fn download_matches_query_matches_status_words() {
    let failed = DownloadEntry {
      download_id: DownloadId(1),
      tab_id: TabId(1),
      url: "https://example.com/file.zip".to_string(),
      file_name: "file.zip".to_string(),
      path: PathBuf::from("/tmp/file.zip"),
      path_display: "/tmp/file.zip".to_string(),
      status: DownloadStatus::Failed {
        error: "disk full".to_string(),
      },
    };
    assert!(download_matches_query(&failed, "failed"));

    let completed = DownloadEntry {
      download_id: DownloadId(2),
      tab_id: TabId(1),
      url: "https://example.com/file.zip".to_string(),
      file_name: "file.zip".to_string(),
      path: PathBuf::from("/tmp/file.zip"),
      path_display: "/tmp/file.zip".to_string(),
      status: DownloadStatus::Completed,
    };
    assert!(download_matches_query(&completed, "completed"));

    let active = DownloadEntry {
      download_id: DownloadId(3),
      tab_id: TabId(1),
      url: "https://example.com/file.zip".to_string(),
      file_name: "file.zip".to_string(),
      path: PathBuf::from("/tmp/file.zip"),
      path_display: "/tmp/file.zip".to_string(),
      status: DownloadStatus::InProgress {
        received_bytes: 10,
        total_bytes: Some(20),
      },
    };
    assert!(download_matches_query(&active, "downloading"));

    let cancelled = DownloadEntry {
      download_id: DownloadId(4),
      tab_id: TabId(1),
      url: "https://example.com/file.zip".to_string(),
      file_name: "file.zip".to_string(),
      path: PathBuf::from("/tmp/file.zip"),
      path_display: "/tmp/file.zip".to_string(),
      status: DownloadStatus::Cancelled,
    };
    assert!(download_matches_query(&cancelled, "cancelled"));
  }

  #[test]
  fn copy_link_action_emits_download_url() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::dark(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    let download_id = DownloadId(42);
    let entry = DownloadEntry {
      download_id,
      tab_id: TabId(1),
      url: "https://example.com/file.zip".to_string(),
      file_name: "file.zip".to_string(),
      path: PathBuf::from("/tmp/file.zip"),
      path_display: "/tmp/file.zip".to_string(),
      status: DownloadStatus::Completed,
    };

    // Frame 0: render once to capture widget ids.
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[entry.clone()],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    let copy_link_id = expect_temp_id(&ctx, ("downloads_copy_link_button_id", download_id.0));

    // Frame 1: focus the copy-link button.
    ctx.memory_mut(|mem| mem.request_focus(copy_link_id));
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[entry.clone()],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(copy_link_id)),
      "expected focus on Copy link button"
    );

    // Frame 2: activate via keyboard.
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let output = downloads_panel_ui(
      &ctx,
      &[entry.clone()],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();

    assert_eq!(output.copy_requests, vec![entry.url.clone()]);
  }

  #[test]
  fn copy_path_action_emits_file_path_string() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::dark(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    let download_id = DownloadId(7);
    let entry = DownloadEntry {
      download_id,
      tab_id: TabId(1),
      url: "https://example.com/file.zip".to_string(),
      file_name: "file.zip".to_string(),
      path: PathBuf::from("downloads/file.zip"),
      path_display: "downloads/file.zip".to_string(),
      status: DownloadStatus::Failed {
        error: "network error".to_string(),
      },
    };

    // Frame 0: render once to capture widget ids.
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[entry.clone()],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    let copy_path_id = expect_temp_id(&ctx, ("downloads_copy_path_button_id", download_id.0));

    // Frame 1: focus the copy-path button.
    ctx.memory_mut(|mem| mem.request_focus(copy_path_id));
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &[entry.clone()],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(copy_path_id)),
      "expected focus on Copy path button"
    );

    // Frame 2: activate via keyboard.
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let output = downloads_panel_ui(
      &ctx,
      &[entry.clone()],
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();

    assert_eq!(output.copy_requests, vec![entry.path_display.clone()]);
  }

  #[test]
  fn clear_completed_button_emits_request() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::dark(None);
    let download_dir = PathBuf::from("test-download-dir");
    let mut search_query = String::new();

    let downloads = vec![
      DownloadEntry {
        download_id: DownloadId(1),
        tab_id: TabId(1),
        url: "https://example.com/file.bin".to_string(),
        file_name: "file.bin".to_string(),
        path: PathBuf::from("downloads/file.bin"),
        path_display: "downloads/file.bin".to_string(),
        status: DownloadStatus::InProgress {
          received_bytes: 10,
          total_bytes: Some(100),
        },
      },
      DownloadEntry {
        download_id: DownloadId(2),
        tab_id: TabId(1),
        url: "https://example.com/file.zip".to_string(),
        file_name: "file.zip".to_string(),
        path: PathBuf::from("downloads/file.zip"),
        path_display: "downloads/file.zip".to_string(),
        status: DownloadStatus::Completed,
      },
    ];

    // Frame 0: render once to capture the clear-completed button id.
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    let clear_id = expect_temp_id(&ctx, "downloads_panel_clear_completed_button_id");

    // Frame 1: focus the clear-completed button.
    ctx.memory_mut(|mem| mem.request_focus(clear_id));
    begin_frame(&ctx, Vec::new());
    let _ = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(clear_id)),
      "expected focus on Clear completed button"
    );

    // Frame 2: activate via keyboard.
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let output = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();

    assert!(output.clear_completed_requested);
  }

  #[test]
  fn downloads_panel_search_escape_clears_then_closes() {
    let ctx = egui::Context::default();
    let theme = BrowserTheme::dark(None);
    let download_dir = PathBuf::from("test-download-dir");
    let downloads: Vec<DownloadEntry> = Vec::new();
    let mut search_query = "report".to_string();

    // Frame 0: render once to capture the search input id and request initial focus.
    begin_frame(&ctx, Vec::new());
    let output = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      true,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(!output.close_requested, "panel should not request close on open");
    let search_id = expect_temp_id(&ctx, "downloads_panel_search_input_id");

    // Frame 1: Escape should clear the query (and not close).
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let output = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(search_id)),
      "expected search field to retain focus after clearing"
    );
    assert!(
      search_query.trim().is_empty(),
      "expected Escape to clear search query"
    );
    assert!(
      !output.close_requested,
      "expected first Escape to clear search, not close panel"
    );

    // Frame 2: Escape again should close (query already empty, nothing left to clear).
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let output = downloads_panel_ui(
      &ctx,
      &downloads,
      &mut search_query,
      &theme,
      false,
      download_dir.as_path(),
    );
    let _ = ctx.end_frame();
    assert!(
      output.close_requested,
      "expected second Escape (with empty query) to request panel close"
    );
  }
}
