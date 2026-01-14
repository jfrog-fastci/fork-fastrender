//! Benchmarks for the egui-based downloads side panel.
//!
//! Run with:
//!   cargo bench --features browser_ui --bench downloads_panel_ui

#[cfg(not(feature = "browser_ui"))]
fn main() {}

#[cfg(feature = "browser_ui")]
use std::path::PathBuf;

#[cfg(feature = "browser_ui")]
use criterion::{black_box, criterion_group, criterion_main, Criterion};

#[cfg(feature = "browser_ui")]
use fastrender::ui::downloads_panel;

#[cfg(feature = "browser_ui")]
use fastrender::ui::theme;

#[cfg(feature = "browser_ui")]
use fastrender::ui::{DownloadEntry, DownloadId, DownloadStatus, TabId};

#[cfg(feature = "browser_ui")]
fn synthetic_downloads(count: usize) -> Vec<DownloadEntry> {
  let mut out = Vec::with_capacity(count);
  for idx in 0..count {
    let status = if idx < 8 {
      DownloadStatus::InProgress {
        received_bytes: (idx as u64) * 1_024 * 128,
        total_bytes: Some(50_000_000),
      }
    } else if idx < 16 {
      DownloadStatus::Failed {
        error: "Network error".to_string(),
      }
    } else {
      DownloadStatus::Completed
    };
    let started_at_ms = Some(idx as u64);
    let finished_at_ms = if matches!(status, DownloadStatus::InProgress { .. }) {
      None
    } else {
      Some(idx as u64 + 1)
    };
    let path = PathBuf::from(format!("/tmp/file-{idx}.bin"));
    let path_display = path.display().to_string();
    out.push(DownloadEntry {
      download_id: DownloadId(idx as u64 + 1),
      tab_id: TabId(1),
      url: format!("https://example.com/file-{idx}.bin"),
      file_name: format!("file-{idx}.bin"),
      path,
      path_display,
      status,
      started_at_ms,
      finished_at_ms,
    });
  }
  out
}

#[cfg(feature = "browser_ui")]
fn bench_downloads_panel_ui(c: &mut Criterion) {
  let ctx = egui::Context::default();
  let theme = fastrender::ui::theme::BrowserTheme::dark(None);
  theme::apply_browser_theme(&ctx, &theme);

  let downloads = synthetic_downloads(2000);
  let mut t = 0.0_f64;

  c.bench_function("downloads_panel_ui_2000_entries", |b| {
    b.iter(|| {
      let mut input = egui::RawInput::default();
      input.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(1280.0, 720.0),
      ));
      input.time = Some(t);
      t += 1.0 / 60.0;

      ctx.begin_frame(input);
      let out = downloads_panel::downloads_panel_ui(
        &ctx,
        black_box(&downloads),
        &theme,
        /*request_initial_focus=*/ false,
      );
      black_box(out);
      let full_output = ctx.end_frame();
      let _ = ctx.tessellate(full_output.shapes);
    });
  });
}

#[cfg(feature = "browser_ui")]
criterion_group!(benches, bench_downloads_panel_ui);

#[cfg(feature = "browser_ui")]
criterion_main!(benches);
