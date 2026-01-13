#![cfg(feature = "browser_ui")]

use std::path::Path;

use super::super::{downloads_panel, theme::BrowserTheme, DownloadEntry};

pub struct DownloadsPanelInput<'a> {
  pub downloads: &'a [DownloadEntry],
  pub search_query: &'a mut String,
  pub theme: &'a BrowserTheme,
  pub request_initial_focus: bool,
  pub download_dir: &'a Path,
}

pub type DownloadsPanelOutput = downloads_panel::DownloadsPanelOutput;

pub fn downloads_panel_ui(
  ctx: &egui::Context,
  input: DownloadsPanelInput<'_>,
) -> DownloadsPanelOutput {
  downloads_panel::downloads_panel_ui(
    ctx,
    input.downloads,
    input.search_query,
    input.theme,
    input.request_initial_focus,
    input.download_dir,
  )
}
