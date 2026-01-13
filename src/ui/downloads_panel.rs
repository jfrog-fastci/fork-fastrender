#![cfg(feature = "browser_ui")]

//! Downloads side panel UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (worker messages, OS open/reveal) are performed by the caller
//! (typically `src/bin/browser.rs`).

use std::path::{Path, PathBuf};

use super::{
  a11y_labels, motion::UiMotion, panel_empty_state, panel_header_with_actions, panel_search_field,
  theme::BrowserTheme, BrowserIcon, DownloadEntry, DownloadId, DownloadStatus, TabId,
};

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
  let prefix = if file_name.is_empty() {
    "Downloading".to_string()
  } else {
    format!("Downloading {file_name}")
  };

  match total_bytes.filter(|t| *t > 0) {
    Some(total) => format!(
      "{prefix}: {} of {}",
      format_bytes(received_bytes),
      format_bytes(total)
    ),
    None => {
      if received_bytes > 0 {
        format!("{prefix}: {}", format_bytes(received_bytes))
      } else {
        prefix
      }
    }
  }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
  if needle.is_empty() {
    return true;
  }
  if haystack.is_empty() {
    return false;
  }

  let haystack = haystack.as_bytes();
  let needle = needle.as_bytes();
  if needle.len() > haystack.len() {
    return false;
  }

  // Naive ASCII case-insensitive substring search. This avoids allocating `to_ascii_lowercase()`
  // strings every frame while still remaining linear in the total length of download strings.
  for start in 0..=haystack.len().saturating_sub(needle.len()) {
    if haystack[start].to_ascii_lowercase() != needle[0].to_ascii_lowercase() {
      continue;
    }
    let mut matched = true;
    for offset in 1..needle.len() {
      if haystack[start + offset].to_ascii_lowercase() != needle[offset].to_ascii_lowercase() {
        matched = false;
        break;
      }
    }
    if matched {
      return true;
    }
  }

  false
}

pub fn download_matches_query(entry: &DownloadEntry, query: &str) -> bool {
  let query = query.trim();
  if query.is_empty() {
    return true;
  }

  contains_ignore_ascii_case(&entry.file_name, query) || contains_ignore_ascii_case(&entry.url, query)
}

#[derive(Debug, Default)]
pub struct DownloadsPanelOutput {
  pub close_requested: bool,
  pub cancel_requests: Vec<(TabId, DownloadId)>,
  pub retry_requests: Vec<(TabId, String)>,
  pub open_requests: Vec<PathBuf>,
  pub reveal_requests: Vec<PathBuf>,
}

#[cfg(test)]
fn store_test_id(ctx: &egui::Context, key: &'static str, id: egui::Id) {
  ctx.data_mut(|d| {
    d.insert_temp(egui::Id::new(key), id);
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

  egui::SidePanel::right("downloads_panel")
    .resizable(true)
    .default_width(360.0)
    .show(ctx, |ui| {
      let header_out = panel_header_with_actions(
        ui,
        BrowserIcon::Download,
        "Downloads",
        |ui| {
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
      // Allow Escape to close the downloads panel when the search field is focused *and* there is
      // nothing left to clear.
      if search_out.response.has_focus()
        && ui.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape))
        && !search_out.cleared
        && search_query.trim().is_empty()
      {
        out.close_requested = true;
      }

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
      let filtered_count = if query.is_empty() {
        None
      } else {
        let matches = downloads
          .iter()
          .rev()
          .filter(|entry| download_matches_query(entry, query))
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
                    egui::RichText::new(entry.path.display().to_string())
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
                          out.retry_requests.push((entry.tab_id, entry.url.clone()));
                        }
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
                          out.retry_requests.push((entry.tab_id, entry.url.clone()));
                        }
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
                    .on_hover_text(err.to_string());
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

          if query.is_empty() {
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
              if !download_matches_query(entry, query) {
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

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

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

  fn expect_temp_id(ctx: &egui::Context, key: &'static str) -> egui::Id {
    ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new(key)))
      .unwrap_or_else(|| panic!("expected temp id {key:?}"))
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
    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::Enter,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::default(),
      }],
    );
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

  fn sample_entry(file_name: &str, url: &str) -> DownloadEntry {
    DownloadEntry {
      download_id: DownloadId(1),
      tab_id: TabId(1),
      url: url.to_string(),
      file_name: file_name.to_string(),
      path: PathBuf::from("/tmp/example"),
      status: DownloadStatus::Completed,
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
}
