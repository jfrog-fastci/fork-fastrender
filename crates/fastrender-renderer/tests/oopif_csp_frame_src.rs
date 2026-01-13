mod common;

use common::{net_test_lock, site_key_for_url, RendererProc, TestResponse, TestServer};
use fastrender_ipc::csp::FrameNode;
use fastrender_ipc::{
  composite_subframes, BrowserToRenderer, NavigationContext, ReferrerPolicy, SiteKeyFactory,
};
use std::time::Duration;

#[test]
fn oopif_parent_csp_frame_src_none_blocks_iframe_creation() {
  let _net_guard = net_test_lock();

  let Some(child_server) = TestServer::start(
    "oopif_parent_csp_frame_src_none_child",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><p>child</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let child_url = child_server.url("frame.html");
  let child_url_for_parent = child_url.clone();

  let Some(parent_server) = TestServer::start(
    "oopif_parent_csp_frame_src_none_parent",
    move |path| match path {
      "/index.html" => Some((
        format!(
          "<!doctype html><html><head>\
           <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'none'\">\
           </head><body>\
           <iframe src=\"{child_url_for_parent}\"></iframe>\
           </body></html>"
        )
        .into_bytes(),
        "text/html",
      )),
      _ => None,
    },
  ) else {
    return;
  };
  let parent_url = parent_server.url("index.html");

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = fastrender_ipc::FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    url: parent_url.clone(),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: site_key_for_url(&parent_url),
      ..Default::default()
    },
  });
  parent_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: parent_frame,
  });

  let ready = parent_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    ready.frame_id, parent_frame,
    "expected FrameReady for parent (err={:?})",
    ready.last_error
  );
  assert!(
    !ready.subframes.is_empty(),
    "expected at least one discovered subframe (err={:?})",
    ready.last_error
  );

  let (committed_url, csp_values) = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  assert!(
    csp_values
      .iter()
      .any(|v| v.contains("frame-src") && v.contains("'none'")),
    "expected renderer to report CSP values via NavigationCommitted, got {csp_values:?}"
  );

  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed_url, csp_values);

  let iframe = &ready.subframes[0];
  let child_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported in SubframeInfo");

  // Browser-side enforcement: parent CSP `frame-src 'none'` blocks creating/navigating the child.
  let diag = node
    .check_frame_src(child_src)
    .expect_err("expected frame-src 'none' to block iframe");
  assert_eq!(
    diag,
    format!("Blocked by Content-Security-Policy (frame-src) for requested URL: {child_src}")
  );

  // Since the child frame was blocked, the browser composites no child surface: the iframe region
  // remains the parent background (i.e. the parent placeholder pixels are unchanged).
  let composed = composite_subframes(
    ready.buffer.clone(),
    std::iter::empty::<(
      &fastrender_ipc::SubframeInfo,
      &fastrender_ipc::FrameBuffer,
    )>(),
  )
  .expect("composite should succeed");
  assert_eq!(
    composed.rgba8, ready.buffer.rgba8,
    "expected blocked iframe to composite as transparent/placeholder"
  );

  // The blocked iframe must not result in any child renderer navigation requests.
  let child_captured = child_server.shutdown_and_join();
  assert!(
    child_captured.is_empty(),
    "expected no network requests to child frame when blocked, got {child_captured:?}"
  );

  parent_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}

#[test]
fn oopif_parent_csp_frame_src_checked_against_final_redirect_url() {
  let _net_guard = net_test_lock();

  let Some(blocked_server) = TestServer::start(
    "oopif_parent_csp_frame_src_redirect_blocked",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><p>blocked</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let blocked_url = blocked_server.url("frame.html");
  let blocked_url_for_redirect = blocked_url.clone();

  let Some(parent_server) = TestServer::start_with(
    "oopif_parent_csp_frame_src_redirect_parent",
    move |path| match path {
      "/index.html" => Some(TestResponse {
        status: 200,
        headers: Vec::new(),
        body: b"<!doctype html><html><head>\
              <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'self'\">\
              </head><body>\
              <iframe src=\"/redir\"></iframe>\
              </body></html>"
          .to_vec(),
        content_type: "text/html",
      }),
      "/redir" => Some(TestResponse {
        status: 302,
        headers: vec![("Location".to_string(), blocked_url_for_redirect.clone())],
        body: Vec::new(),
        content_type: "text/plain",
      }),
      _ => None,
    },
  ) else {
    return;
  };
  let parent_url = parent_server.url("index.html");

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = fastrender_ipc::FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    url: parent_url.clone(),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: parent_site_key.clone(),
      ..Default::default()
    },
  });
  parent_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: parent_frame,
  });

  let ready = parent_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    ready.frame_id, parent_frame,
    "expected FrameReady for parent (err={:?})",
    ready.last_error
  );
  assert!(
    !ready.subframes.is_empty(),
    "expected at least one discovered subframe (err={:?})",
    ready.last_error
  );

  let (committed_url, csp_values) = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed_url, csp_values);

  let iframe = &ready.subframes[0];
  let requested_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported in SubframeInfo");

  // Pre-navigation check: requested URL (same-origin "/redir") passes `frame-src 'self'`.
  let resolved_requested = node
    .check_frame_src(requested_src)
    .expect("expected parent CSP to allow requested iframe URL");

  // Spawn the OOPIF renderer and navigate it to the requested URL; the renderer will follow the
  // redirect and commit the final URL.
  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  let child_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    resolved_requested.as_str(),
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    iframe,
  );
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    url: resolved_requested.to_string(),
    context: child_ctx,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });

  let child_ready = child_renderer.recv_frame_ready(Duration::from_secs(10));
  let (child_committed_url, _child_csp) = child_ready
    .last_committed
    .clone()
    .expect("expected child NavigationCommitted");
  assert_eq!(
    child_committed_url, blocked_url,
    "expected child navigation to commit the redirected URL"
  );

  // Post-navigation check: the final redirected URL must also satisfy the parent's CSP.
  let diag = node
    .check_frame_src_with_final(requested_src, Some(child_committed_url.as_str()))
    .expect_err("expected parent CSP to block final redirect destination");
  assert_eq!(
    diag,
    format!(
      "Blocked by Content-Security-Policy (frame-src) for final URL: {child_committed_url}"
    )
  );

  // Blocked iframe: compositor must not blend the child surface (placeholder remains).
  let composed = composite_subframes(
    ready.buffer.clone(),
    std::iter::empty::<(
      &fastrender_ipc::SubframeInfo,
      &fastrender_ipc::FrameBuffer,
    )>(),
  )
  .expect("composite should succeed");
  assert_eq!(
    composed.rgba8, ready.buffer.rgba8,
    "expected blocked redirect iframe to composite as transparent/placeholder"
  );

  // The redirect should have been followed (request reached the blocked server), but the browser
  // must still treat the navigation as blocked.
  blocked_server.wait_for_request(
    |req| req.path == "/frame.html",
    "expected redirected navigation to fetch /frame.html",
  );

  parent_renderer.shutdown();
  child_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
  let _ = blocked_server.shutdown_and_join();
}
