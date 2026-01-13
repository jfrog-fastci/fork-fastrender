mod common;

use common::RendererProc;
use fastrender_ipc::{
  site_key_for_navigation, BrowserToRenderer, FrameId, NavigationContext, SiteIsolationMode,
  SiteLock,
};
use std::time::Duration;

#[test]
fn renderer_stdio_rejects_cross_site_navigation_with_site_lock() {
  let mut renderer = RendererProc::spawn();

  let frame = FrameId(1);

  let locked_site = site_key_for_navigation("https://a.test/", None);
  let lock = SiteLock::from_site_key(&locked_site, SiteIsolationMode::PerOrigin);
  renderer.send(&BrowserToRenderer::SetSiteLock { lock });

  renderer.send(&BrowserToRenderer::CreateFrame { frame_id: frame });
  renderer.send(&BrowserToRenderer::Resize {
    frame_id: frame,
    width: 1,
    height: 1,
    dpr: 1.0,
  });
  renderer.send(&BrowserToRenderer::RequestRepaint { frame_id: frame });
  let baseline = renderer.recv_frame_ready(Duration::from_secs(5));
  assert_eq!(
    baseline.frame_id, frame,
    "expected FrameReady before site-lock violation (err={:?})",
    baseline.last_error
  );

  let disallowed_url = "https://b.test/".to_string();
  // Simulate buggy browser: keeps the old site_key but changes the URL cross-site.
  renderer.send(&BrowserToRenderer::Navigate {
    frame_id: frame,
    url: disallowed_url.clone(),
    context: NavigationContext {
      site_key: locked_site,
      ..Default::default()
    },
  });

  let (failed_frame, failed_url, failed_error) = renderer
    .recv_navigation_failed(Duration::from_secs(5))
    .expect("expected NavigationFailed due to site lock");
  assert_eq!(failed_frame, frame);
  assert_eq!(failed_url, disallowed_url);
  assert!(
    failed_error.contains("site lock"),
    "unexpected NavigationFailed error: {failed_error}"
  );

  // Subsequent renders must not reflect the rejected navigation.
  renderer.send(&BrowserToRenderer::RequestRepaint { frame_id: frame });
  let after = renderer.recv_frame_ready(Duration::from_secs(5));
  assert_eq!(
    after.frame_id, frame,
    "expected FrameReady after site-lock violation (err={:?})",
    after.last_error
  );
  assert_eq!(
    after.buffer.rgba8, baseline.buffer.rgba8,
    "rejected navigation must not affect renderer output"
  );

  renderer.shutdown();
}

