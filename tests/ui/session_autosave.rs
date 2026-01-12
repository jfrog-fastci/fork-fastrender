use fastrender::ui::about_pages;
use fastrender::ui::session::{load_session, save_session_atomic};
use fastrender::ui::{BrowserSession, SessionAutosave};
use std::time::Duration;

#[test]
fn startup_creates_minimal_session_when_missing() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.json");
  assert!(!path.exists());

  let autosave = SessionAutosave::new(path.clone());
  autosave.flush(Duration::from_secs(2)).unwrap();

  let session = load_session(&path).unwrap().unwrap();
  assert_eq!(session.version, 2);
  assert!(!session.did_exit_cleanly);
  assert_eq!(session.windows.len(), 1);
  assert_eq!(session.windows[0].tabs.len(), 1);
  assert_eq!(session.windows[0].tabs[0].url, about_pages::ABOUT_NEWTAB);
  assert_eq!(session.windows[0].active_tab_index, 0);
}

#[test]
fn debounce_persists_latest_snapshot() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.json");

  let autosave = SessionAutosave::new(path.clone());
  autosave.flush(Duration::from_secs(2)).unwrap();

  autosave.request_save(BrowserSession::single("about:blank".to_string()));
  autosave.request_save(BrowserSession::single("about:newtab".to_string()));
  autosave.request_save(BrowserSession::single("about:error".to_string()));

  // Default debounce is 500ms; wait a little longer for the write to land.
  std::thread::sleep(Duration::from_millis(700));
  autosave.flush(Duration::from_secs(2)).unwrap();

  let session = load_session(&path).unwrap().unwrap();
  assert_eq!(session.windows.len(), 1);
  assert_eq!(
    session.windows[0].tabs[0].url,
    "about:error",
    "expected only the final snapshot to be persisted"
  );
  assert!(!session.did_exit_cleanly, "expected running sessions to be unclean");
}

#[test]
fn crash_marker_toggles_unclean_then_clean() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.json");

  let initial = BrowserSession::single("about:blank".to_string());
  save_session_atomic(&path, &initial).unwrap();

  let mut autosave = SessionAutosave::new(path.clone());
  autosave.flush(Duration::from_secs(2)).unwrap();

  let session = load_session(&path).unwrap().unwrap();
  assert!(!session.did_exit_cleanly, "startup should mark session as unclean");

  autosave.shutdown(Duration::from_secs(2)).unwrap();
  let session = load_session(&path).unwrap().unwrap();
  assert!(session.did_exit_cleanly, "clean shutdown should mark session as clean");
}

#[test]
fn drop_does_not_mark_session_clean() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.json");

  {
    let autosave = SessionAutosave::new(path.clone());
    autosave.request_save(BrowserSession::single("about:blank".to_string()));
    autosave.flush(Duration::from_secs(2)).unwrap();
    // Drop without calling `shutdown()`: should *not* mark the session as clean.
  }

  let session = load_session(&path).unwrap().unwrap();
  assert!(
    !session.did_exit_cleanly,
    "dropping SessionAutosave should not mark the session as clean"
  );
}
