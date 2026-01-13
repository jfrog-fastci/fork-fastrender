#![cfg(feature = "browser_ui")]

pub mod bookmarks_manager;
pub mod clear_browsing_data_dialog;
pub mod downloads_panel;
pub mod home_url_dialog;
pub mod history_panel;

pub use bookmarks_manager::{bookmarks_manager_side_panel, BookmarksManagerInput, BookmarksManagerOutput};
pub use clear_browsing_data_dialog::{
  clear_browsing_data_dialog_ui, ClearBrowsingDataDialogInput, ClearBrowsingDataDialogOutput,
};
pub use downloads_panel::{downloads_panel_ui, DownloadsPanelInput, DownloadsPanelOutput};
pub use home_url_dialog::{home_url_dialog_ui, HomeUrlDialogInput, HomeUrlDialogOutput};
pub use history_panel::{history_panel_ui, HistoryPanelInput, HistoryPanelOutput};
