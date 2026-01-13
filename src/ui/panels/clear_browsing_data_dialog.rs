#![cfg(feature = "browser_ui")]

use super::super::{clear_browsing_data_dialog, ClearBrowsingDataRange};

pub struct ClearBrowsingDataDialogInput<'a> {
  pub open: &'a mut bool,
  pub range: &'a mut ClearBrowsingDataRange,
}

pub type ClearBrowsingDataDialogOutput = clear_browsing_data_dialog::ClearBrowsingDataDialogOutput;

pub fn clear_browsing_data_dialog_ui(
  ctx: &egui::Context,
  input: ClearBrowsingDataDialogInput<'_>,
) -> ClearBrowsingDataDialogOutput {
  clear_browsing_data_dialog::clear_browsing_data_dialog_ui(ctx, input.open, input.range)
}

