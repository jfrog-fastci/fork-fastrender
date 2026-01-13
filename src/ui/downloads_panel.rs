#![cfg(feature = "browser_ui")]

//! Downloads side panel UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (worker messages, OS open/reveal) are performed by the caller
//! (typically `src/bin/browser.rs`).

use std::path::PathBuf;

use super::{
  a11y_labels, downloads, icon, icon_button, icon_tinted,
  motion::UiMotion,
  theme::BrowserTheme,
  BrowserIcon, DownloadEntry, DownloadId, DownloadStatus, TabId,
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
    Some(total) => format!("{prefix}: {} of {}", format_bytes(received_bytes), format_bytes(total)),
    None => {
      if received_bytes > 0 {
        format!("{prefix}: {}", format_bytes(received_bytes))
      } else {
        prefix
      }
    }
  }
}

#[derive(Debug, Default)]
pub struct DownloadsPanelOutput {
  pub close_requested: bool,
  pub cancel_requests: Vec<(TabId, DownloadId)>,
  pub retry_requests: Vec<(TabId, String)>,
  pub open_requests: Vec<PathBuf>,
  pub reveal_requests: Vec<PathBuf>,
}

pub fn downloads_panel_ui(
  ctx: &egui::Context,
  downloads: &[DownloadEntry],
  theme: &BrowserTheme,
  request_initial_focus: bool,
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
    egui::Stroke::new(a.width + (b.width - a.width) * t, lerp_color(a.color, b.color, t))
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
      ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        icon(ui, BrowserIcon::Download, ui.spacing().icon_width);
        ui.heading("Downloads");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          let close_resp = icon_button(ui, BrowserIcon::Close, "Close (Esc)", true);
          close_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close downloads panel")
          });
          if request_initial_focus {
            close_resp.request_focus();
          }
          if close_resp.clicked() {
            out.close_requested = true;
          }

          let show_folder = ui.small_button("Show downloads folder");
          show_folder.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Show downloads folder")
          });
          if show_folder.clicked() {
            out.open_requests.push(downloads::default_download_dir());
          }
        });
      });
      ui.separator();

      if downloads.is_empty() {
        ui.centered_and_justified(|ui| {
          ui.vertical_centered(|ui| {
            let tint = ui.visuals().weak_text_color();
            icon_tinted(ui, BrowserIcon::Download, 28.0, tint);
            ui.add_space(10.0);
            ui.label(egui::RichText::new("No downloads yet").strong());
          });
        });
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

      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
          ui.spacing_mut().item_spacing.y = row_gap;

          let body_h = ui.text_style_height(&egui::TextStyle::Body);
          let small_h = ui.text_style_height(&egui::TextStyle::Small);
          // Conservatively estimate the progress bar height so rows look consistent even if egui's
          // internal widget sizing changes slightly between versions.
          let progress_h = (ui.spacing().interact_size.y * 0.42).clamp(8.0, 12.0);
          let line_gap = (theme.sizing.padding * 0.25).clamp(2.0, 4.0);

          for entry in downloads.iter().rev() {
            let has_progress = matches!(entry.status, DownloadStatus::InProgress { .. });
            let has_error = matches!(
              entry.status,
              DownloadStatus::Failed { ref error } if !error.trim().is_empty()
            );

            let mut content_h = body_h + line_gap + small_h + line_gap + small_h;
            if has_error {
              content_h += line_gap + small_h;
            }
            if has_progress {
              content_h += line_gap + progress_h;
            }
            let row_height = (content_h + row_padding * 2.0).ceil();

            let row_id = egui::Id::new(("fastr_download_row", entry.download_id.0));
            let (rect, _response) = ui.allocate_exact_size(
              egui::vec2(ui.available_width(), row_height),
              egui::Sense::hover(),
            );

            let hover_t = motion.animate_bool(
              ui.ctx(),
              row_id.with("hover"),
              ui.ctx().input(|i| {
                i.pointer
                  .hover_pos()
                  .is_some_and(|pos| rect.contains(pos))
              }),
              motion.durations.hover_fade,
            );

            let base_fill = visuals.widgets.inactive.bg_fill;
            let base_stroke = visuals.widgets.noninteractive.bg_stroke;
            let hover_stroke = visuals.widgets.hovered.bg_stroke;

            ui.painter().rect_filled(rect, row_rounding, base_fill);
            if hover_t > 0.0 {
              ui
                .painter()
                .rect_filled(rect, row_rounding, with_scaled_alpha(hover_overlay, hover_t));
            }
            ui.painter().rect_stroke(
              rect,
              row_rounding,
              lerp_stroke(base_stroke, hover_stroke, hover_t),
            );

            let inner_rect = rect.shrink(row_padding);
            ui.allocate_ui_at_rect(inner_rect, |ui| {
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
                DownloadStatus::Failed { .. } => ("Failed".to_string(), ui.visuals().error_fg_color, false),
              };

              ui.horizontal(|ui| {
                ui.add(
                  egui::Label::new(egui::RichText::new(status_text).small().color(status_color))
                    .wrap(false)
                    .truncate(true),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                  match &entry.status {
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
                  }
                });
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
          }
        });
    });

  out
}

#[cfg(test)]
mod tests {
  use super::download_progress_a11y_label;

  #[test]
  fn download_progress_a11y_label_contains_file_name() {
    let label = download_progress_a11y_label("example.zip", 1_024, Some(2_048));
    assert!(!label.trim().is_empty(), "label should not be empty");
    assert!(
      label.contains("example.zip"),
      "expected label to mention file name; got {label:?}"
    );
  }
}
