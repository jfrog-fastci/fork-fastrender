mod common;

use common::{net_test_lock, tiny_png, RendererProc, TestServer};
use fastrender_ipc::{
  BrowserToRenderer, FrameId, NavigationContext, ReferrerPolicy, SiteKeyFactory,
};
use std::time::Duration;

#[test]
fn oopif_iframe_referrerpolicy_no_referrer_omits_referer_on_child_subresource_fetches() {
  let _net_guard = net_test_lock();

  let Some(child_server) = TestServer::start(
    "oopif_iframe_referrerpolicy_no_referrer_child",
    |path| match path {
      "/frame.html" => Some((
        b"<!doctype html><html><body><img src=\"/img.png\"></body></html>".to_vec(),
        "text/html",
      )),
      "/img.png" => Some((tiny_png(), "image/png")),
      _ => None,
    },
  ) else {
    return;
  };
  let child_url = child_server.url("frame.html");

  let child_url_for_parent = child_url.clone();
  let Some(parent_server) = TestServer::start(
    "oopif_iframe_referrerpolicy_no_referrer_parent",
    move |path| match path {
      "/index.html" => Some((
        format!(
          "<!doctype html><html><body><iframe src=\"{child_url_for_parent}\" referrerpolicy=\"no-referrer\"></iframe></body></html>"
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
    "expected FrameReady for parent frame (err={:?})",
    ready.last_error
  );
  assert!(
    !ready.subframes.is_empty(),
    "expected parent renderer to report at least one subframe (err={:?})",
    ready.last_error
  );

  let iframe = &ready.subframes[0];
  assert_eq!(iframe.referrer_policy, Some(ReferrerPolicy::NoReferrer));
  assert_eq!(iframe.sandbox_flags, Default::default());
  assert!(
    !iframe.opaque_origin,
    "expected iframe without sandbox to not force opaque origin"
  );
  assert_eq!(
    iframe.src.as_deref(),
    Some(child_url.as_str()),
    "expected subframe src to round-trip through IPC"
  );

  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  let child_site_key = site_keys.site_key_for_navigation(&child_url, Some(&parent_site_key));
  let child_context = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    &child_url,
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    iframe,
  );
  assert_eq!(child_context.referrer_policy, ReferrerPolicy::NoReferrer);
  assert_eq!(child_context.referrer_url.as_deref(), Some(parent_url.as_str()));
  assert_eq!(child_context.site_key, child_site_key);
  assert_eq!(child_context.sandbox_flags, iframe.sandbox_flags);
  assert_eq!(child_context.opaque_origin, iframe.opaque_origin);
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    url: child_url.clone(),
    context: child_context,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });

  // The child renderer should fetch /img.png without sending a Referer header due to the iframe's
  // `referrerpolicy=no-referrer` attribute.
  child_server.wait_for_request(
    |req| req.path == "/img.png",
    "expected child process to fetch /img.png",
  );
  let captured = child_server.shutdown_and_join();
  let img_requests: Vec<_> = captured.iter().filter(|r| r.path == "/img.png").collect();
  assert!(
    !img_requests.is_empty(),
    "expected at least one request for /img.png, got: {captured:?}"
  );
  for req in img_requests {
    assert_eq!(req.referer, None, "unexpected Referer header: {req:?}");
  }

  // Stop renderers and parent server.
  parent_renderer.shutdown();
  child_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}

#[test]
fn oopif_about_blank_site_key_inherits_parent_unless_sandboxed() {
  let _net_guard = net_test_lock();

  let Some(parent_server) = TestServer::start(
    "oopif_about_blank_site_key_inherits_parent_unless_sandboxed",
    |_path| Some((
      b"<!doctype html><html><body>\
        <iframe src=\"about:blank\"></iframe>\
        <iframe sandbox src=\"about:blank\"></iframe>\
      </body></html>"
        .to_vec(),
      "text/html",
    )),
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
    "expected FrameReady for parent frame (err={:?})",
    ready.last_error
  );
  assert_eq!(
    ready.subframes.len(),
    2,
    "expected two iframe subframes (err={:?}, subframes={:#?})",
    ready.last_error,
    ready.subframes
  );

  // Sort deterministically by child id so the test does not depend on renderer enumeration order.
  let mut subframes = ready.subframes;
  subframes.sort_by_key(|s| s.child.raw());

  let unsandboxed = &subframes[0];
  assert!(
    !unsandboxed.opaque_origin,
    "unsandboxed about:blank should not force opaque origin"
  );
  let unsandboxed_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    "about:blank",
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    unsandboxed,
  );
  assert_eq!(
    unsandboxed_ctx.site_key, parent_site_key,
    "about:blank should inherit parent site key when not sandboxed"
  );
  assert!(!unsandboxed_ctx.opaque_origin);

  let sandboxed = &subframes[1];
  assert!(
    sandboxed.opaque_origin,
    "sandboxed about:blank should force opaque origin"
  );
  let sandboxed_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    "about:blank",
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    sandboxed,
  );
  assert!(
    sandboxed_ctx.site_key != parent_site_key,
    "sandboxed about:blank should not inherit parent site key"
  );
  assert!(sandboxed_ctx.opaque_origin);
  assert_eq!(sandboxed_ctx.sandbox_flags, sandboxed.sandbox_flags);

  parent_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}

#[test]
fn oopif_srcdoc_site_key_inherits_parent_unless_sandboxed() {
  let _net_guard = net_test_lock();

  let Some(parent_server) = TestServer::start(
    "oopif_srcdoc_site_key_inherits_parent_unless_sandboxed",
    |_path| {
      Some((
        b"<!doctype html><html><body>\
          <iframe src=\"https://cross.example/\" srcdoc=\"<p>hello</p>\"></iframe>\
          <iframe sandbox src=\"https://cross.example/\" srcdoc=\"<p>hello</p>\"></iframe>\
        </body></html>"
          .to_vec(),
        "text/html",
      ))
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
    "expected FrameReady for parent frame (err={:?})",
    ready.last_error
  );
  assert_eq!(
    ready.subframes.len(),
    2,
    "expected two iframe subframes (err={:?}, subframes={:#?})",
    ready.last_error,
    ready.subframes
  );

  let mut subframes = ready.subframes;
  subframes.sort_by_key(|s| s.child.raw());

  let unsandboxed = &subframes[0];
  assert!(
    !unsandboxed.opaque_origin,
    "unsandboxed srcdoc should not force opaque origin"
  );
  let unsandboxed_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    "about:srcdoc",
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    unsandboxed,
  );
  assert_eq!(
    unsandboxed_ctx.site_key, parent_site_key,
    "about:srcdoc should inherit parent site key when not sandboxed"
  );
  assert!(!unsandboxed_ctx.opaque_origin);

  let sandboxed = &subframes[1];
  assert!(
    sandboxed.opaque_origin,
    "sandboxed srcdoc should force opaque origin"
  );
  let sandboxed_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    "about:srcdoc",
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    sandboxed,
  );
  assert!(
    sandboxed_ctx.site_key != parent_site_key,
    "sandboxed about:srcdoc should not inherit parent site key"
  );
  assert!(sandboxed_ctx.opaque_origin);

  parent_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}
