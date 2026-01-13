//! Pure formatting helpers for user-facing "Clear browsing data" confirmations.
//!
//! Kept UI-framework agnostic so it can be unit tested without compiling the optional egui/winit
//! stack (unlike the dialog UI itself).

use super::ClearBrowsingDataRange;

/// Format the chrome toast shown after browsing data is cleared.
///
/// When `removed_entries` is provided, the toast includes a stable entry count. Otherwise it falls
/// back to a generic confirmation message.
pub fn format_clear_browsing_data_toast(
  range: ClearBrowsingDataRange,
  removed_entries: Option<usize>,
) -> String {
  let range_label = range.label();
  match removed_entries {
    Some(count) => {
      let noun = if count == 1 { "entry" } else { "entries" };
      format!("Cleared browsing data: removed {count} {noun} ({range_label})")
    }
    None => format!("Cleared browsing data ({range_label})"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn range_labels_match_clear_browsing_data_dialog() {
    assert_eq!(
      format_clear_browsing_data_toast(ClearBrowsingDataRange::LastHour, None),
      "Cleared browsing data (Last hour)"
    );
    assert_eq!(
      format_clear_browsing_data_toast(ClearBrowsingDataRange::Last24Hours, None),
      "Cleared browsing data (Last 24 hours)"
    );
    assert_eq!(
      format_clear_browsing_data_toast(ClearBrowsingDataRange::Last7Days, None),
      "Cleared browsing data (Last 7 days)"
    );
    assert_eq!(
      format_clear_browsing_data_toast(ClearBrowsingDataRange::AllTime, None),
      "Cleared browsing data (All time)"
    );
  }

  #[test]
  fn removed_entry_count_formatting_is_stable() {
    assert_eq!(
      format_clear_browsing_data_toast(ClearBrowsingDataRange::Last24Hours, Some(0)),
      "Cleared browsing data: removed 0 entries (Last 24 hours)"
    );
    assert_eq!(
      format_clear_browsing_data_toast(ClearBrowsingDataRange::Last24Hours, Some(1)),
      "Cleared browsing data: removed 1 entry (Last 24 hours)"
    );
    assert_eq!(
      format_clear_browsing_data_toast(ClearBrowsingDataRange::Last24Hours, Some(2)),
      "Cleared browsing data: removed 2 entries (Last 24 hours)"
    );
  }
}

