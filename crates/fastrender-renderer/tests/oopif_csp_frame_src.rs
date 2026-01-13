mod common;

use common::{net_test_lock, RendererProc, TestResponse, TestServer};
use fastrender_ipc::csp::FrameNode;
use fastrender_ipc::{
  composite_subframes, BrowserToRenderer, FrameId, IframeNavigation, NavigationContext, ReferrerPolicy,
  SiteKeyFactory,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
  let child_src_for_parent = child_url
    .strip_prefix("http:")
    .unwrap_or(child_url.as_str())
    .to_string();
  let child_src_for_parent_for_server = child_src_for_parent.clone();

  let Some(parent_server) = TestServer::start(
    "oopif_parent_csp_frame_src_none_parent",
    move |path| match path {
      "/index.html" => Some((
        format!(
          "<!doctype html><html><head>\
           <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'none'\">\
           </head><body>\
           <iframe src=\"{child_src_for_parent_for_server}\"></iframe>\
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

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    navigation: IframeNavigation::Url(parent_url.clone()),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: parent_site_key,
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

  let committed = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  assert!(
    committed
      .csp
      .iter()
      .any(|v| v.contains("frame-src") && v.contains("'none'")),
    "expected renderer to report CSP values via NavigationCommitted, got {:?}",
    committed.csp
  );

  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed.url, committed.csp);
  if let Some(base_url) = committed.base_url {
    node.set_base_url(base_url);
  }

  let iframe = &ready.subframes[0];
  let child_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported in SubframeInfo");
  assert_eq!(
    child_src, child_src_for_parent,
    "expected SubframeInfo to report raw <iframe src> value"
  );

  // Browser-side enforcement: parent CSP `frame-src 'none'` blocks creating/navigating the child.
  let diag = node
    .check_frame_src(child_src)
    .expect_err("expected frame-src 'none' to block iframe");
  assert_eq!(
    diag,
    format!("Blocked by Content-Security-Policy (frame-src) for requested URL: {child_url}")
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
fn oopif_parent_csp_src_change_to_blocked_url_does_not_navigate_child() {
  let _net_guard = net_test_lock();

  let Some(blocked_server) = TestServer::start(
    "oopif_parent_csp_src_change_blocked_child",
    |path| match path {
      "/blocked.html" => Some((b"<!doctype html><p>blocked</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let blocked_url = blocked_server.url("blocked.html");
  let blocked_src = blocked_url
    .strip_prefix("http:")
    .unwrap_or(blocked_url.as_str())
    .to_string();

  let index_counter = Arc::new(AtomicUsize::new(0));
  let index_counter_for_server = Arc::clone(&index_counter);
  let blocked_src_for_server = blocked_src.clone();

  let Some(server) = TestServer::start(
    "oopif_parent_csp_src_change_parent",
    move |path| match path {
      "/index.html" => {
        let idx = index_counter_for_server.fetch_add(1, Ordering::SeqCst);
        let html = if idx == 0 {
          r#"<!doctype html><html><head>
              <meta http-equiv="Content-Security-Policy" content="frame-src 'self'">
              </head><body>
              <iframe src="/frame.html"></iframe>
              </body></html>"#
            .to_string()
        } else {
          format!(
            "<!doctype html><html><head>\
             <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'self'\">\
             </head><body>\
             <iframe src=\"{blocked_src_for_server}\"></iframe>\
             </body></html>"
          )
        };
        Some((html.into_bytes(), "text/html"))
      }
      "/frame.html" => Some((b"<!doctype html><p>child</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };

  let parent_url = server.url("index.html");
  let expected_allowed_child_url = server.url("frame.html");

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    navigation: IframeNavigation::Url(parent_url.clone()),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: parent_site_key.clone(),
      ..Default::default()
    },
  });

  // First paint: iframe src is same-origin /frame.html (allowed by frame-src 'self').
  parent_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: parent_frame,
  });
  let ready1 = parent_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    ready1.frame_id, parent_frame,
    "expected FrameReady for parent (err={:?})",
    ready1.last_error
  );
  assert_eq!(ready1.subframes.len(), 1);
  let committed1 = ready1
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed1.url, committed1.csp);
  if let Some(base_url) = committed1.base_url {
    node.set_base_url(base_url);
  }

  let iframe1 = &ready1.subframes[0];
  let src1 = iframe1
    .src
    .as_deref()
    .expect("expected iframe src");
  assert_eq!(src1, "/frame.html");
  let resolved_allowed = node
    .check_frame_src(src1)
    .expect("expected initial iframe to be allowed by frame-src 'self'");
  assert_eq!(resolved_allowed.as_str(), expected_allowed_child_url);

  // Spawn a child renderer and navigate to the allowed URL.
  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe1.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  child_renderer.send(&BrowserToRenderer::Resize {
    frame_id: child_frame,
    width: 1,
    height: 1,
    dpr: 1.0,
  });
  let child_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    resolved_allowed.as_str(),
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    iframe1,
  );
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    navigation: IframeNavigation::Url(resolved_allowed.to_string()),
    context: child_ctx,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });
  let child_ready = child_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    child_ready.frame_id, child_frame,
    "expected FrameReady for child (err={:?})",
    child_ready.last_error
  );
  server.wait_for_request(
    |req| req.path == "/frame.html",
    "expected child renderer to fetch /frame.html",
  );

  // Second paint: parent HTML changes iframe src to a cross-origin URL; the browser must block the
  // navigation and keep the existing child content.
  parent_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: parent_frame,
  });
  let ready2 = parent_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    ready2.frame_id, parent_frame,
    "expected FrameReady for parent after src change (err={:?})",
    ready2.last_error
  );
  assert_eq!(ready2.subframes.len(), 1);
  let committed2 = ready2
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  node.navigation_committed(committed2.url, committed2.csp);
  if let Some(base_url) = committed2.base_url {
    node.set_base_url(base_url);
  }

  let iframe2 = &ready2.subframes[0];
  assert_eq!(iframe2.child, child_frame, "iframe FrameId should be stable");
  let src2 = iframe2
    .src
    .as_deref()
    .expect("expected iframe src after update");
  assert_eq!(src2, blocked_src, "expected raw src to reflect updated HTML");

  let diag = node
    .check_frame_src(src2)
    .expect_err("expected updated iframe src to be blocked by frame-src 'self'");
  assert_eq!(
    diag,
    format!("Blocked by Content-Security-Policy (frame-src) for requested URL: {blocked_url}")
  );

  // The browser should continue presenting the existing child surface (no navigation).
  let composed1 = composite_subframes(
    ready1.buffer.clone(),
    std::iter::once((iframe1, &child_ready.buffer)),
  )
  .expect("composite first paint");
  let composed2 = composite_subframes(
    ready2.buffer.clone(),
    std::iter::once((iframe2, &child_ready.buffer)),
  )
  .expect("composite second paint");
  assert_eq!(
    composed2.rgba8, composed1.rgba8,
    "expected blocked src change to keep previous iframe content"
  );

  // The blocked server must not be contacted (browser refused to navigate).
  let blocked_captured = blocked_server.shutdown_and_join();
  assert!(
    blocked_captured.is_empty(),
    "expected no network requests to blocked iframe URL, got {blocked_captured:?}"
  );

  child_renderer.shutdown();
  parent_renderer.shutdown();
  let _ = server.shutdown_and_join();
}

#[test]
fn oopif_parent_csp_src_change_to_allowed_url_navigates_child() {
  let _net_guard = net_test_lock();

  let index_counter = Arc::new(AtomicUsize::new(0));
  let index_counter_for_server = Arc::clone(&index_counter);

  let Some(server) = TestServer::start(
    "oopif_parent_csp_src_change_allowed_parent",
    move |path| match path {
      "/index.html" => {
        let idx = index_counter_for_server.fetch_add(1, Ordering::SeqCst);
        let iframe_src = if idx == 0 { "/frame1.html" } else { "/frame2.html" };
        Some((
          format!(
            "<!doctype html><html><head>\
             <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'self'\">\
             </head><body>\
             <iframe src=\"{iframe_src}\"></iframe>\
             </body></html>"
          )
          .into_bytes(),
          "text/html",
        ))
      }
      "/frame1.html" => Some((b"<!doctype html><p>child1</p>".to_vec(), "text/html")),
      "/frame2.html" => Some((b"<!doctype html><p>child2</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };

  let parent_url = server.url("index.html");
  let expected_child_url1 = server.url("frame1.html");
  let expected_child_url2 = server.url("frame2.html");

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    navigation: IframeNavigation::Url(parent_url.clone()),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: parent_site_key.clone(),
      ..Default::default()
    },
  });

  // First paint: iframe src is /frame1.html.
  parent_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: parent_frame,
  });
  let ready1 = parent_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    ready1.frame_id, parent_frame,
    "expected FrameReady for parent (err={:?})",
    ready1.last_error
  );
  assert_eq!(ready1.subframes.len(), 1);
  let committed1 = ready1
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed1.url, committed1.csp);
  if let Some(base_url) = committed1.base_url {
    node.set_base_url(base_url);
  }

  let iframe1 = &ready1.subframes[0];
  let src1 = iframe1.src.as_deref().expect("expected iframe src");
  assert_eq!(src1, "/frame1.html");
  let resolved1 = node
    .check_frame_src(src1)
    .expect("expected /frame1.html to be allowed by frame-src 'self'");
  assert_eq!(resolved1.as_str(), expected_child_url1);

  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe1.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  child_renderer.send(&BrowserToRenderer::Resize {
    frame_id: child_frame,
    width: 1,
    height: 1,
    dpr: 1.0,
  });
  let child_ctx1 = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    resolved1.as_str(),
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    iframe1,
  );
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    navigation: IframeNavigation::Url(resolved1.to_string()),
    context: child_ctx1,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });
  let child_ready1 = child_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    child_ready1.frame_id, child_frame,
    "expected FrameReady for child (err={:?})",
    child_ready1.last_error
  );

  // Second paint: iframe src changes to /frame2.html (still same-origin, allowed).
  parent_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: parent_frame,
  });
  let ready2 = parent_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    ready2.frame_id, parent_frame,
    "expected FrameReady for parent after src change (err={:?})",
    ready2.last_error
  );
  assert_eq!(ready2.subframes.len(), 1);
  let committed2 = ready2
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  node.navigation_committed(committed2.url, committed2.csp);
  if let Some(base_url) = committed2.base_url {
    node.set_base_url(base_url);
  }

  let iframe2 = &ready2.subframes[0];
  assert_eq!(
    iframe2.child, child_frame,
    "iframe FrameId should be stable across src changes"
  );
  let src2 = iframe2.src.as_deref().expect("expected updated iframe src");
  assert_eq!(src2, "/frame2.html");
  let resolved2 = node
    .check_frame_src(src2)
    .expect("expected /frame2.html to be allowed by frame-src 'self'");
  assert_eq!(resolved2.as_str(), expected_child_url2);

  let child_ctx2 = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    resolved2.as_str(),
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    iframe2,
  );
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    navigation: IframeNavigation::Url(resolved2.to_string()),
    context: child_ctx2,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });
  let child_ready2 = child_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    child_ready2.frame_id, child_frame,
    "expected FrameReady for child after src change (err={:?})",
    child_ready2.last_error
  );

  // The two navigations should result in distinct placeholder buffers (URL hash changes).
  assert_ne!(
    child_ready1.buffer.rgba8, child_ready2.buffer.rgba8,
    "expected child buffer to change after navigating to a different URL"
  );

  // Browser should now composite with the updated child surface.
  let composed1 = composite_subframes(
    ready1.buffer.clone(),
    std::iter::once((iframe1, &child_ready1.buffer)),
  )
  .expect("composite first paint");
  let composed2 = composite_subframes(
    ready2.buffer.clone(),
    std::iter::once((iframe2, &child_ready2.buffer)),
  )
  .expect("composite second paint");
  assert_ne!(
    composed1.rgba8, composed2.rgba8,
    "expected composited output to change after allowed iframe navigation"
  );

  // Ensure the second child navigation produced an actual HTTP request to /frame2.html.
  server.wait_for_request(
    |req| req.path == "/frame2.html",
    "expected child renderer to fetch /frame2.html after iframe src change",
  );

  child_renderer.shutdown();
  parent_renderer.shutdown();
  let _ = server.shutdown_and_join();
}

#[test]
fn oopif_parent_csp_default_src_none_blocks_iframe_creation() {
  let _net_guard = net_test_lock();

  let Some(child_server) = TestServer::start(
    "oopif_parent_csp_default_src_none_child",
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
    "oopif_parent_csp_default_src_none_parent",
    move |path| match path {
      "/index.html" => Some((
        format!(
          "<!doctype html><html><head>\
           <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'\">\
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

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    navigation: IframeNavigation::Url(parent_url.clone()),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: parent_site_key,
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

  let committed = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  assert!(
    committed
      .csp
      .iter()
      .any(|v| v.contains("default-src") && v.contains("'none'")),
    "expected renderer to report CSP values via NavigationCommitted, got {:?}",
    committed.csp
  );

  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed.url, committed.csp);
  if let Some(base_url) = committed.base_url {
    node.set_base_url(base_url);
  }

  let iframe = &ready.subframes[0];
  let child_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported in SubframeInfo");

  let diag = node
    .check_frame_src(child_src)
    .expect_err("expected default-src 'none' to block iframe via frame-src fallback");
  assert_eq!(
    diag,
    format!("Blocked by Content-Security-Policy (frame-src) for requested URL: {child_url}")
  );

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

  let child_captured = child_server.shutdown_and_join();
  assert!(
    child_captured.is_empty(),
    "expected no network requests to child frame when blocked, got {child_captured:?}"
  );

  parent_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}

#[test]
fn oopif_parent_csp_multiple_policies_intersect_to_block_iframe() {
  let _net_guard = net_test_lock();

  let Some(child_server) = TestServer::start(
    "oopif_parent_csp_multiple_policies_child",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><p>child</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let child_url = child_server.url("frame.html");

  let child_url_for_parent = child_url.clone();
  let Some(parent_server) = TestServer::start_with(
    "oopif_parent_csp_multiple_policies_parent",
    move |path| match path {
      "/index.html" => Some(TestResponse {
        status: 200,
        headers: vec![("Content-Security-Policy".to_string(), "frame-src *".to_string())],
        body: format!(
          "<!doctype html><html><head>\
           <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'none'\">\
           </head><body>\
           <iframe src=\"{child_url_for_parent}\"></iframe>\
           </body></html>"
        )
        .into_bytes(),
        content_type: "text/html",
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
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    navigation: IframeNavigation::Url(parent_url.clone()),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: parent_site_key,
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

  let committed = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  assert!(
    committed
      .csp
      .iter()
      .any(|v| v.contains("frame-src") && v.contains("*")),
    "expected CSP header value to be reported, got {:?}",
    committed.csp
  );
  assert!(
    committed
      .csp
      .iter()
      .any(|v| v.contains("frame-src") && v.contains("'none'")),
    "expected CSP meta value to be reported, got {:?}",
    committed.csp
  );

  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed.url, committed.csp);
  if let Some(base_url) = committed.base_url {
    node.set_base_url(base_url);
  }

  let iframe = &ready.subframes[0];
  let child_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported in SubframeInfo");

  // Multiple CSP directive sets combine by intersection: `frame-src *` AND `frame-src 'none'` must
  // both allow the navigation (so the iframe should be blocked).
  let diag = node
    .check_frame_src(child_src)
    .expect_err("expected CSP intersection to block iframe");
  assert_eq!(
    diag,
    format!("Blocked by Content-Security-Policy (frame-src) for requested URL: {child_url}")
  );

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
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    navigation: IframeNavigation::Url(parent_url.clone()),
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

  let committed = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed.url, committed.csp);
  if let Some(base_url) = committed.base_url {
    node.set_base_url(base_url);
  }

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
  child_renderer.send(&BrowserToRenderer::Resize {
    frame_id: child_frame,
    width: 1,
    height: 1,
    dpr: 1.0,
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
    navigation: IframeNavigation::Url(resolved_requested.to_string()),
    context: child_ctx,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });

  let child_ready = child_renderer.recv_frame_ready(Duration::from_secs(10));
  let child_committed_url = child_ready
    .last_committed
    .clone()
    .expect("expected child NavigationCommitted")
    .url;
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

#[test]
fn oopif_parent_csp_frame_src_self_allows_same_origin_iframe_navigation() {
  let _net_guard = net_test_lock();

  let Some(server) = TestServer::start(
    "oopif_parent_csp_frame_src_self_allows_same_origin",
    |path| match path {
      "/index.html" => Some((
        b"<!doctype html><html><head>\
          <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'self'\">\
          </head><body>\
          <iframe src=\"/frame.html\"></iframe>\
          </body></html>"
          .to_vec(),
        "text/html",
      )),
      "/frame.html" => Some((b"<!doctype html><p>child</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };

  let parent_url = server.url("index.html");
  let expected_child_url = server.url("frame.html");

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    navigation: IframeNavigation::Url(parent_url.clone()),
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

  let committed = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  assert!(
    committed
      .csp
      .iter()
      .any(|v| v.contains("frame-src") && v.contains("'self'")),
    "expected renderer to report CSP values via NavigationCommitted, got {:?}",
    committed.csp
  );

  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed.url, committed.csp);
  if let Some(base_url) = committed.base_url {
    node.set_base_url(base_url);
  }

  let iframe = &ready.subframes[0];
  let child_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported in SubframeInfo");
  assert_eq!(child_src, "/frame.html");

  let resolved = node
    .check_frame_src(child_src)
    .expect("expected frame-src 'self' to allow same-origin iframe navigation");
  assert_eq!(resolved.as_str(), expected_child_url);

  // Browser-side OOPIF implementation: spawn a separate child renderer process and navigate it.
  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  child_renderer.send(&BrowserToRenderer::Resize {
    frame_id: child_frame,
    width: 1,
    height: 1,
    dpr: 1.0,
  });
  let child_context = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    resolved.as_str(),
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    iframe,
  );
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    navigation: IframeNavigation::Url(resolved.to_string()),
    context: child_context,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });

  let child_ready = child_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(
    child_ready.frame_id, child_frame,
    "expected FrameReady for child (err={:?})",
    child_ready.last_error
  );

  // The child navigation should have resulted in a real HTTP request to /frame.html.
  server.wait_for_request(
    |req| req.path == "/frame.html",
    "expected child renderer to fetch iframe document",
  );

  // Compositing the child buffer should replace the parent's top-left pixel (identity transform).
  let composed = composite_subframes(
    ready.buffer.clone(),
    std::iter::once((&ready.subframes[0], &child_ready.buffer)),
  )
  .expect("composite should succeed");
  assert_eq!(
    &composed.rgba8[0..4],
    &child_ready.buffer.rgba8[0..4],
    "expected child buffer to be composited over parent"
  );
  assert_eq!(
    &composed.rgba8[4..],
    &ready.buffer.rgba8[4..],
    "expected remaining parent pixels to remain unchanged"
  );

  child_renderer.shutdown();
  parent_renderer.shutdown();
  let _ = server.shutdown_and_join();
}

#[test]
fn oopif_parent_csp_base_href_affects_iframe_src_resolution() {
  let _net_guard = net_test_lock();

  let Some(blocked_server) = TestServer::start(
    "oopif_parent_csp_base_href_blocked_server",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><p>blocked</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let blocked_base = blocked_server.url("");
  let blocked_url = blocked_server.url("frame.html");

  let blocked_base_for_parent = blocked_base.clone();
  let Some(parent_server) = TestServer::start(
    "oopif_parent_csp_base_href_parent",
    move |path| match path {
      "/index.html" => Some((
        format!(
          "<!doctype html><html><head>\
           <meta http-equiv=\"Content-Security-Policy\" content=\"frame-src 'self'\">\
           <base href=\"{blocked_base_for_parent}\">\
           </head><body>\
           <iframe src=\"frame.html\"></iframe>\
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

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 2,
    height: 2,
    dpr: 1.0,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    url: parent_url.clone(),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: parent_site_key,
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
  assert_eq!(ready.subframes.len(), 1);

  let committed = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  assert_eq!(
    committed.base_url.as_deref(),
    Some(blocked_base.as_str()),
    "expected renderer to report <base href> via NavigationCommitted"
  );

  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed.url, committed.csp);
  if let Some(base_url) = committed.base_url {
    node.set_base_url(base_url);
  }

  let iframe = &ready.subframes[0];
  let child_src = iframe.src.as_deref().expect("expected iframe src");
  assert_eq!(child_src, "frame.html");

  // With `<base href>` set to a different origin, a relative iframe `src` must resolve against the
  // base URL and therefore be blocked by `frame-src 'self'`.
  let diag = node
    .check_frame_src(child_src)
    .expect_err("expected base href to affect CSP iframe URL resolution");
  assert_eq!(
    diag,
    format!("Blocked by Content-Security-Policy (frame-src) for requested URL: {blocked_url}")
  );

  let blocked_captured = blocked_server.shutdown_and_join();
  assert!(
    blocked_captured.is_empty(),
    "expected no fetches to blocked base origin when browser blocks navigation, got {blocked_captured:?}"
  );

  parent_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}
