use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GlobalHistoryEntry {
  pub url: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  /// Unix epoch milliseconds.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub visited_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GlobalHistoryStore {
  #[serde(default)]
  pub entries: Vec<GlobalHistoryEntry>,
}

impl GlobalHistoryStore {
  pub fn record(&mut self, url: String, title: Option<String>) {
    // Deduplicate consecutive duplicates (common when rapidly committing the same URL).
    if self.entries.last().is_some_and(|e| e.url == url) {
      if let Some(last) = self.entries.last_mut() {
        last.title = title;
        last.visited_at_ms = Some(now_unix_ms());
      }
      return;
    }

    self.entries.push(GlobalHistoryEntry {
      url,
      title,
      visited_at_ms: Some(now_unix_ms()),
    });
  }

  pub fn clear(&mut self) {
    self.entries.clear();
  }
}

fn now_unix_ms() -> u64 {
  use std::time::{SystemTime, UNIX_EPOCH};

  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .unwrap_or(0)
}

