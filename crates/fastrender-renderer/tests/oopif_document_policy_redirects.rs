mod common;

use common::{net_test_lock, RendererProc, TestResponse, TestServer};
use fastrender_ipc::csp::FrameNode;
use fastrender_ipc::{BrowserToRenderer, NavigationContext, ReferrerPolicy, SiteKeyFactory};
use std::time::Duration;

#[test]
fn oopif_parent_policy_blocks_mixed_content_redirect_final_url() {
  let _net_guard = net_test_lock();

  let Some(insecure_server) = TestServer::start(
    "oopif_parent_policy_mixed_content_insecure_child",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><p>insecure</p>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let insecure_url = insecure_server.url("frame.html");
  let insecure_url_for_redirect = insecure_url.clone();

  let Some(parent_server) = TestServer::start_with(
    "oopif_parent_policy_mixed_content_parent",
    move |path| match path {
      "/index.html" => Some(TestResponse {
        status: 200,
        headers: Vec::new(),
        body: b"<!doctype html><html><body><iframe src=\"/redir\"></iframe></body></html>"
          .to_vec(),
        content_type: "text/html",
      }),
      "/redir" => Some(TestResponse {
        status: 302,
        headers: vec![("Location".to_string(), insecure_url_for_redirect.clone())],
        body: Vec::new(),
        content_type: "text/plain",
      }),
      _ => None,
    },
  ) else {
    return;
  };

  // Treat the parent document as HTTPS so we can exercise mixed-content blocking, but still serve
  // it from the local plain HTTP test server.
  let parent_url = parent_server
    .url("index.html")
    .replacen("http://", "https://", 1);

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
    "expected parent renderer to report at least one subframe (err={:?})",
    ready.last_error
  );

  let (committed_url, csp_values) = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed_url, csp_values);
  // Mirror the embedder's policy: HTTPS document blocks mixed HTTP content.
  node.set_resource_policy(false, true);

  let iframe = &ready.subframes[0];
  let requested_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported");

  // Pre-navigation policy check (requested URL is https://.../redir, so allowed).
  node
    .check_document_policy(requested_src)
    .expect("expected embedder policy to allow requested iframe URL");

  // Resolve the requested URL against the parent base URL (like the browser would).
  let resolved_requested = node
    .check_frame_src(requested_src)
    .expect("expected URL resolution to succeed");

  // Navigate the child frame. The renderer follows redirects and reports the final committed URL.
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
    child_committed_url, insecure_url,
    "expected child navigation to commit the redirected URL"
  );

  // Post-navigation enforcement: the final redirected URL must satisfy the parent's document policy.
  let err = node
    .check_document_policy_with_final(requested_src, Some(child_committed_url.as_str()))
    .expect_err("expected mixed-content policy to block final redirect destination");
  assert_eq!(err, "Blocked mixed HTTP content from HTTPS document");

  insecure_server.wait_for_request(
    |req| req.path == "/frame.html",
    "expected redirected navigation to fetch /frame.html",
  );

  parent_renderer.shutdown();
  child_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
  let _ = insecure_server.shutdown_and_join();
}

#[test]
fn oopif_parent_policy_blocks_file_redirect_final_url() {
  let _net_guard = net_test_lock();

  const FILE_URL: &str = "file:///etc/passwd";

  let Some(parent_server) = TestServer::start_with(
    "oopif_parent_policy_file_redirect_parent",
    |path| match path {
      "/index.html" => Some(TestResponse {
        status: 200,
        headers: Vec::new(),
        body: b"<!doctype html><html><body><iframe src=\"/redir\"></iframe></body></html>"
          .to_vec(),
        content_type: "text/html",
      }),
      "/redir" => Some(TestResponse {
        status: 302,
        headers: vec![("Location".to_string(), FILE_URL.to_string())],
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
    "expected parent renderer to report at least one subframe (err={:?})",
    ready.last_error
  );

  let (committed_url, csp_values) = ready
    .last_committed
    .clone()
    .expect("expected NavigationCommitted before FrameReady");
  let mut node = FrameNode::new(parent_frame);
  node.navigation_committed(committed_url, csp_values);
  // Default allow_file_from_http is false. No mixed-content blocking needed for this test.
  node.set_resource_policy(false, false);

  let iframe = &ready.subframes[0];
  let requested_src = iframe
    .src
    .as_deref()
    .expect("expected iframe src to be reported");

  // Pre-navigation policy check passes (`/redir` is http://...).
  node
    .check_document_policy(requested_src)
    .expect("expected embedder policy to allow requested iframe URL");

  let resolved_requested = node
    .check_frame_src(requested_src)
    .expect("expected URL resolution to succeed");

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
    child_committed_url, FILE_URL,
    "expected child navigation to commit the redirected file:// URL"
  );

  let err = node
    .check_document_policy_with_final(requested_src, Some(child_committed_url.as_str()))
    .expect_err("expected file:// policy to block final redirect destination");
  assert_eq!(err, "Blocked file:// resource from HTTP(S) document");

  // Ensure the redirecting endpoint was actually requested.
  parent_server.wait_for_request(
    |req| req.path == "/redir",
    "expected child navigation to request /redir",
  );

  parent_renderer.shutdown();
  child_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}
