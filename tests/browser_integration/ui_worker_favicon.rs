#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, recv_for_tab, viewport_changed_msg, TempSite, DEFAULT_TIMEOUT};
use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;

#[test]
fn ui_worker_emits_favicon_for_link_rel_icon() {
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();

  // Create a simple PNG icon dynamically so the test stays hermetic.
  let icon = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
    64,
    64,
    image::Rgba([0x00, 0x80, 0xff, 0xff]),
  ));
  let mut icon_bytes: Vec<u8> = Vec::new();
  icon
    .write_to(
      &mut std::io::Cursor::new(&mut icon_bytes),
      image::ImageFormat::Png,
    )
    .expect("encode png");
  std::fs::write(site.dir.path().join("icon.png"), icon_bytes).expect("write icon.png");

  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <title>Icon page</title>
    <link rel="icon" href="icon.png">
  </head>
  <body>hi</body>
</html>"#,
  );

  let handle = spawn_ui_worker("ui_worker_favicon").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx
    .send(create_tab_msg(tab, Some(page_url)))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab, (128, 96), 1.0))
    .expect("viewport changed");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab })
    .expect("set active tab");
  ui_tx
    .send(UiToWorker::RequestRepaint {
      tab_id: tab,
      reason: RepaintReason::Explicit,
    })
    .expect("request repaint");

  let msg = recv_for_tab(&ui_rx, tab, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::Favicon { .. })
  })
  .expect("expected WorkerToUi::Favicon");

  match msg {
    WorkerToUi::Favicon {
      width,
      height,
      rgba,
      ..
    } => {
      assert_eq!((width, height), (32, 32));
      assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4);
    }
    other => panic!("expected Favicon, got {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

