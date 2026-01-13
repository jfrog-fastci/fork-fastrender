#![cfg(feature = "browser_ui")]

use super::super::{history_panel, GlobalHistoryStore};

pub struct HistoryPanelInput<'a> {
  pub history: &'a GlobalHistoryStore,
  pub search_text: &'a mut String,
  pub request_focus_search: &'a mut bool,
}

pub type HistoryPanelOutput = history_panel::HistoryPanelOutput;

pub fn history_panel_ui(ctx: &egui::Context, input: HistoryPanelInput<'_>) -> HistoryPanelOutput {
  history_panel::history_panel_ui(ctx, input.history, input.search_text, input.request_focus_search)
}

