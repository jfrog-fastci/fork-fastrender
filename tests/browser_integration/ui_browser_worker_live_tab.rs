#![cfg(feature = "browser_ui")]

use fastrender::api::FastRenderFactory;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation;
use fastrender::ui::browser_worker::BrowserWorker;
use fastrender::ui::messages::{TabId, WorkerToUi};
use fastrender::RenderOptions;
use std::time::Duration;
use tempfile::tempdir;

fn recv_frame(rx: &std::sync::mpsc::Receiver<WorkerToUi>) -> fastrender::ui::messages::RenderedFrame {
  loop {
    match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(WorkerToUi::FrameReady { frame, .. }) => return frame,
      Ok(_other) => continue,
      Err(err) => panic!("timed out waiting for FrameReady: {err}"),
    }
  }
}

fn run_on_render_stack(f: impl FnOnce() + Send + 'static) {
  // The full render pipeline can be stack-heavy (layout/paint). Mirror the production UI by
  // running the test logic on a thread with the larger render stack.
  std::thread::Builder::new()
    .name("fastr-ui-browser-worker-live-tab-test".to_string())
    .stack_size(fastrender::system::DEFAULT_RENDER_STACK_SIZE)
    .spawn(f)
    .expect("spawn test thread")
    .join()
    .expect("test thread panicked");
}

#[test]
fn navigation_creates_a_live_tab_and_ticks_are_safe() {
  run_on_render_stack(|| {
    let _stage_listener_guard = crate::browser_integration::stage_listener_test_lock();

    let factory = FastRenderFactory::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
    let mut worker = BrowserWorker::new(factory, tx);

    let tab = TabId(1);

    // Ticking an unknown tab should be a no-op.
    worker.tick(tab).unwrap();
    assert!(!worker.has_tab(tab));

    worker
      .navigate(tab, "about:blank", RenderOptions::default().with_viewport(32, 32))
      .unwrap();
    assert!(worker.has_tab(tab));

    let _frame = recv_frame(&rx);
    // Drain any stage/debug messages emitted during navigation so we only observe messages caused by
    // the tick below.
    for _ in rx.try_iter() {}

    // A clean tab should not repaint on tick.
    worker.tick(tab).unwrap();
    assert!(
      !matches!(rx.recv_timeout(Duration::from_millis(50)), Ok(WorkerToUi::FrameReady { .. })),
      "expected no FrameReady after tick on a clean tab"
    );
  });
}

#[test]
fn second_frame_is_emitted_after_a_timer_mutates_the_dom() {
  run_on_render_stack(|| {
    let _stage_listener_guard = crate::browser_integration::stage_listener_test_lock();

    let factory = FastRenderFactory::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
    let mut worker = BrowserWorker::new(factory, tx);

    let dir = tempdir().unwrap();
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #box { width: 64px; height: 64px; }
            .a { background: rgb(255, 0, 0); }
            .b { background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <div id="box" class="a"></div>
        </body>
      </html>"#;
    std::fs::write(dir.path().join("index.html"), html).unwrap();

    let url = format!("file://{}/index.html", dir.path().display());
    let tab = TabId(1);
    worker
      .navigate(tab, &url, RenderOptions::default().with_viewport(64, 64))
      .unwrap();

    let frame1 = recv_frame(&rx);
    let bytes1 = frame1.pixmap.data().to_vec();

    worker
      .schedule_dom_mutation_timeout(tab, Duration::from_millis(0), |dom| {
        let mut index = DomIndex::build(dom);
        let node_id = *index
          .id_by_element_id
          .get("box")
          .expect("expected #box element");
        index
          .with_node_mut(node_id, |node| dom_mutation::set_attr(node, "class", "b"))
          .unwrap_or(false)
      })
      .unwrap();

    worker.tick(tab).unwrap();
    let frame2 = recv_frame(&rx);
    let bytes2 = frame2.pixmap.data().to_vec();

    assert_ne!(bytes1, bytes2, "expected pixmap to change after timer mutation");
  });
}
