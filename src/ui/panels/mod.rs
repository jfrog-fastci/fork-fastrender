#![cfg(feature = "browser_ui")]

pub mod clear_browsing_data_dialog;
pub mod downloads_panel;
pub mod history_panel;

pub use clear_browsing_data_dialog::{clear_browsing_data_dialog_ui, ClearBrowsingDataDialogInput};
pub use downloads_panel::{downloads_panel_ui, DownloadsPanelInput};
pub use history_panel::{history_panel_ui, HistoryPanelInput};

