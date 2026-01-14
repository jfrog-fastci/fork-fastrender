use std::collections::HashMap;
use std::path::PathBuf;

use fastrender::ui::{
  DownloadId, DownloadOutcome, DownloadStatus, DownloadsState, TabId, WorkerToUi,
};

fn by_id(state: &DownloadsState) -> HashMap<DownloadId, fastrender::ui::DownloadEntry> {
  state
    .downloads
    .iter()
    .cloned()
    .map(|entry| (entry.download_id, entry))
    .collect()
}

#[test]
fn global_downloads_merge_is_order_independent_across_windows() {
  // Simulate two independent UI windows/workers producing download messages. The browser event loop
  // merges them into a single profile-global downloads store keyed by `DownloadId`.
  //
  // This relies on `DownloadId` being process-unique across workers (see `NEXT_DOWNLOAD_ID`).
  let tab_a = TabId(100);
  let tab_b = TabId(200);
  let download_a = DownloadId(1_000);
  let download_b = DownloadId(2_000);

  let msgs_a = vec![
    WorkerToUi::DownloadStarted {
      tab_id: tab_a,
      download_id: download_a,
      url: "https://a.test/file".to_string(),
      file_name: "a.bin".to_string(),
      path: PathBuf::from("a.bin"),
      total_bytes: Some(10),
    },
    WorkerToUi::DownloadProgress {
      tab_id: tab_a,
      download_id: download_a,
      received_bytes: 5,
      total_bytes: Some(10),
    },
    WorkerToUi::DownloadFinished {
      tab_id: tab_a,
      download_id: download_a,
      outcome: DownloadOutcome::Completed,
    },
  ];

  let msgs_b = vec![
    WorkerToUi::DownloadStarted {
      tab_id: tab_b,
      download_id: download_b,
      url: "https://b.test/file".to_string(),
      file_name: "b.bin".to_string(),
      path: PathBuf::from("b.bin"),
      total_bytes: None,
    },
    WorkerToUi::DownloadProgress {
      tab_id: tab_b,
      download_id: download_b,
      received_bytes: 123,
      total_bytes: None,
    },
    WorkerToUi::DownloadFinished {
      tab_id: tab_b,
      download_id: download_b,
      outcome: DownloadOutcome::Failed {
        error: "network error".to_string(),
      },
    },
  ];

  // Order 1: interleave the two window streams.
  let mut state_1 = DownloadsState::default();
  for msg in [&msgs_a[0], &msgs_b[0], &msgs_a[1], &msgs_b[1], &msgs_a[2], &msgs_b[2]] {
    state_1.apply_worker_msg(msg);
  }

  // Order 2: opposite interleaving.
  let mut state_2 = DownloadsState::default();
  for msg in [&msgs_b[0], &msgs_a[0], &msgs_b[1], &msgs_a[1], &msgs_b[2], &msgs_a[2]] {
    state_2.apply_worker_msg(msg);
  }

  let map_1 = by_id(&state_1);
  let map_2 = by_id(&state_2);
  assert_eq!(map_1, map_2);

  assert_eq!(map_1.len(), 2);
  assert!(matches!(
    map_1.get(&download_a).unwrap().status,
    DownloadStatus::Completed
  ));
  assert!(matches!(
    map_1.get(&download_b).unwrap().status,
    DownloadStatus::Failed { .. }
  ));
}

