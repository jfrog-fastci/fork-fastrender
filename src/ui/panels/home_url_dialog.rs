#![cfg(feature = "browser_ui")]

use super::super::home_url_dialog;

pub struct HomeUrlDialogInput<'a> {
  pub open: &'a mut bool,
  pub url_text: &'a mut String,
  pub error: &'a mut Option<String>,
  pub request_initial_focus: &'a mut bool,
}

pub type HomeUrlDialogOutput = home_url_dialog::HomeUrlDialogOutput;

pub fn home_url_dialog_ui(
  ctx: &egui::Context,
  input: HomeUrlDialogInput<'_>,
) -> HomeUrlDialogOutput {
  home_url_dialog::home_url_dialog_ui(
    ctx,
    input.open,
    input.url_text,
    input.error,
    input.request_initial_focus,
  )
}
