#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::multiprocess::{MultiprocessBrowser, RendererToBrowser};

#[test]
fn shared_renderer_process_crash_marks_all_attached_tabs_crashed_and_recovers() {
  let _lock = crate::browser_integration::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url_a = site.write("a.html", "<!doctype html><html><body>a</body></html>");
  let url_b = site.write("b.html", "<!doctype html><html><body>b</body></html>");

  let mut browser = MultiprocessBrowser::new();

  let tab_a = browser.open_tab(&url_a).expect("open tab A");
  let tab_b = browser.open_tab(&url_b).expect("open tab B");

  let proc_a = browser.process_for_tab(tab_a).expect("tab A has process");
  let proc_b = browser.process_for_tab(tab_b).expect("tab B has process");
  assert_eq!(
    proc_a, proc_b,
    "expected process-per-site reuse for tabs with same SiteKey"
  );

  let root_a = browser.root_frame(tab_a).expect("tab A root frame");
  let root_b = browser.root_frame(tab_b).expect("tab B root frame");
  assert_eq!(
    browser.process_attached_frames(proc_a),
    vec![root_a, root_b],
    "expected both root frames to be attached to the shared renderer process"
  );

  assert!(
    browser.handle_renderer_message(proc_a, RendererToBrowser::FrameReady { frame_id: root_a }),
    "expected messages from a live process to be accepted"
  );

  browser.crash_process(proc_a);

  assert!(
    !browser.process_is_alive(proc_a),
    "expected crashed process to be marked dead"
  );

  assert!(browser.tab_is_crashed(tab_a), "tab A should be crashed");
  assert!(browser.tab_is_crashed(tab_b), "tab B should be crashed");
  assert!(browser.frame_is_crashed(root_a), "root frame A should be crashed");
  assert!(browser.frame_is_crashed(root_b), "root frame B should be crashed");

  assert!(
    !browser.handle_renderer_message(proc_a, RendererToBrowser::FrameReady { frame_id: root_a }),
    "expected messages from a dead process to be ignored"
  );

  browser.reload_tab(tab_a).expect("reload tab A");
  assert!(!browser.tab_is_crashed(tab_a), "tab A should recover after reload");

  let proc_after_reload = browser.process_for_tab(tab_a).expect("tab A has process after reload");
  assert_ne!(
    proc_after_reload, proc_a,
    "expected reload to spawn a fresh renderer process for the same SiteKey"
  );
  assert!(
    browser.process_is_alive(proc_after_reload),
    "expected new process to be alive"
  );

  assert!(
    browser.tab_is_crashed(tab_b),
    "tab B should remain crashed until explicitly reloaded"
  );
  assert!(
    browser.process_for_tab(tab_b).is_none(),
    "crashed tab B should not be attached to a renderer process"
  );

  browser.reload_tab(tab_b).expect("reload tab B");
  assert!(!browser.tab_is_crashed(tab_b), "tab B should recover after reload");
  assert_eq!(
    browser.process_for_tab(tab_b),
    Some(proc_after_reload),
    "expected process-per-site reuse when reloading another tab with the same SiteKey"
  );
  assert_eq!(
    browser.process_attached_frames(proc_after_reload),
    vec![root_a, root_b],
    "expected both tabs to be attached to the new shared renderer process after reload"
  );
}

