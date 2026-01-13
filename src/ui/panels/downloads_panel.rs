#![cfg(feature = "browser_ui")]

use super::super::{downloads_panel, theme::BrowserTheme, DownloadEntry};

pub struct DownloadsPanelInput<'a> {
  pub downloads: &'a [DownloadEntry],
  pub theme: &'a BrowserTheme,
  pub request_initial_focus: bool,
}

pub type DownloadsPanelOutput = downloads_panel::DownloadsPanelOutput;

pub fn downloads_panel_ui(ctx: &egui::Context, input: DownloadsPanelInput<'_>) -> DownloadsPanelOutput {
  downloads_panel::downloads_panel_ui(
    ctx,
    input.downloads,
    input.theme,
    input.request_initial_focus,
  )
}

