use fastrender::ui::{load_downloads, DownloadStatus, LoadSource};

#[test]
fn integration_load_downloads_from_injected_json() {
  let dir = tempfile::tempdir().unwrap();
  let file_path = dir.path().join("fastrender_downloads.json");

  let download_path = dir.path().join("example.bin");
  let json = format!(
    r#"{{
      "version": 1,
      "entries": [
        {{
          "url": "https://example.com/example.bin",
          "file_name": "example.bin",
          "path": {},
          "status": "completed",
          "started_at_ms": 10,
          "finished_at_ms": 20
        }}
      ]
    }}"#,
    serde_json::to_string(&download_path).unwrap()
  );

  std::fs::write(&file_path, json).unwrap();

  let outcome = load_downloads(&file_path).unwrap();
  assert_eq!(outcome.source, LoadSource::Disk);
  assert_eq!(outcome.value.downloads.len(), 1);
  let entry = &outcome.value.downloads[0];
  assert_eq!(entry.url, "https://example.com/example.bin");
  assert_eq!(entry.file_name, "example.bin");
  assert_eq!(entry.path, download_path);
  assert!(matches!(entry.status, DownloadStatus::Completed));
  assert_eq!(entry.started_at_ms, Some(10));
  assert_eq!(entry.finished_at_ms, Some(20));
  // Restored downloads are not associated with a live tab.
  assert_eq!(entry.tab_id.0, 0);
  // Download id should be non-zero and stable within the process.
  assert_ne!(entry.download_id.0, 0);
}

