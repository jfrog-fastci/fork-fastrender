mod common;

use common::{net_test_lock, RendererProc, TestServer};
use fastrender_ipc::{
  BrowserToRenderer, CursorKind, FrameHitTester, FrameId, HoverRouter, IframeNavigation,
  NavigationContext, ReferrerPolicy, SiteKeyFactory,
};
use std::time::{Duration, Instant};

fn drive_hover_until(
  procs: &[&RendererProc],
  router: &mut HoverRouter,
  ui_state: &mut fastrender_ipc::HoverState,
  expected: CursorKind,
) {
  let deadline = Instant::now() + Duration::from_secs(3);
  while Instant::now() < deadline {
    if ui_state.cursor == expected {
      return;
    }

    for proc in procs {
      if let Some((frame_id, seq, hovered_url, cursor)) =
        proc.recv_hover_changed(Duration::from_millis(10))
      {
        if let Some(effective) = router.on_hover_changed(frame_id, seq, hovered_url, cursor) {
          *ui_state = effective;
        }
      }
    }
  }
  panic!(
    "timed out waiting for cursor {expected:?} (last effective hover={ui_state:?}, hit_frame={:?})",
    router.hit_frame()
  );
}

#[test]
fn oopif_cursor_uses_deepest_frame_hover_state() {
  let _net_guard = net_test_lock();

  let Some(child_server) = TestServer::start(
    "oopif_cursor_uses_deepest_frame_hover_state_child",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><html><body><input></body></html>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let child_url = child_server.url("frame.html");

  let child_url_for_parent = child_url.clone();
  let Some(parent_server) = TestServer::start(
    "oopif_cursor_uses_deepest_frame_hover_state_parent",
    move |path| match path {
      "/index.html" => Some((
        format!("<!doctype html><html><body><iframe src=\"{child_url_for_parent}\"></iframe></body></html>")
          .into_bytes(),
        "text/html",
      )),
      _ => None,
    },
  ) else {
    let _ = child_server.shutdown_and_join();
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
    width: 100,
    height: 100,
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
    "expected FrameReady for parent frame (err={:?})",
    ready.last_error
  );
  assert!(
    !ready.subframes.is_empty(),
    "expected parent to report at least one subframe (err={:?})",
    ready.last_error
  );
  let iframe = ready.subframes[0].clone();

  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  child_renderer.send(&BrowserToRenderer::Resize {
    frame_id: child_frame,
    width: 50,
    height: 50,
    dpr: 1.0,
  });
  let child_context = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    &child_url,
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    &iframe,
  );
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    navigation: IframeNavigation::Url(child_url.clone()),
    context: child_context,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });
  let _ = child_renderer.recv_frame_ready(Duration::from_secs(10));

  let mut hit_tester = FrameHitTester::new(parent_frame);
  hit_tester.set_frame_size(parent_frame, 100, 100);
  hit_tester.set_frame_size(child_frame, 50, 50);
  hit_tester.set_subframes(parent_frame, vec![iframe]);

  let mut router = HoverRouter::new(parent_frame);
  let mut ui_state = fastrender_ipc::HoverState::default();
  let mut seq: u64 = 1;

  // Outside the iframe region => root frame wins => default cursor.
  {
    let (targets, emit) = router.on_pointer_move(&hit_tester, 75.0, 75.0);
    if let Some(state) = emit {
      ui_state = state;
    }
    let event_seq = seq;
    seq += 1;
    for (frame_id, x_css, y_css) in targets {
      let msg = BrowserToRenderer::PointerMove {
        frame_id,
        x_css,
        y_css,
        seq: event_seq,
      };
      parent_renderer.send(&msg);
    }
    drive_hover_until(
      &[&parent_renderer, &child_renderer],
      &mut router,
      &mut ui_state,
      CursorKind::Default,
    );
    assert_eq!(ui_state.hovered_url, None);
  }

  // Inside the iframe region => child frame wins => text cursor.
  {
    let (targets, emit) = router.on_pointer_move(&hit_tester, 10.0, 10.0);
    if let Some(state) = emit {
      ui_state = state;
    }
    let event_seq = seq;
    seq += 1;
    for (frame_id, x_css, y_css) in targets {
      let msg = BrowserToRenderer::PointerMove {
        frame_id,
        x_css,
        y_css,
        seq: event_seq,
      };
      if frame_id == parent_frame {
        parent_renderer.send(&msg);
      } else if frame_id == child_frame {
        child_renderer.send(&msg);
      } else {
        panic!("unexpected frame id in pointer targets: {frame_id:?}");
      }
    }
    drive_hover_until(
      &[&parent_renderer, &child_renderer],
      &mut router,
      &mut ui_state,
      CursorKind::Text,
    );
  }

  // Leaving the iframe should revert to the root's cursor.
  {
    let (targets, emit) = router.on_pointer_move(&hit_tester, 75.0, 75.0);
    if let Some(state) = emit {
      ui_state = state;
    }
    let event_seq = seq;
    seq += 1;
    for (frame_id, x_css, y_css) in targets {
      let msg = BrowserToRenderer::PointerMove {
        frame_id,
        x_css,
        y_css,
        seq: event_seq,
      };
      parent_renderer.send(&msg);
    }
    drive_hover_until(
      &[&parent_renderer, &child_renderer],
      &mut router,
      &mut ui_state,
      CursorKind::Default,
    );
  }

  // Stop renderers and servers.
  let _ = seq;
  parent_renderer.shutdown();
  child_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
  let _ = child_server.shutdown_and_join();
}

#[test]
fn oopif_hovered_url_uses_deepest_frame() {
  let _net_guard = net_test_lock();

  let Some(child_server) = TestServer::start(
    "oopif_hovered_url_uses_deepest_frame_child",
    |path| match path {
      "/frame.html" => Some((
        b"<!doctype html><html><body><a href=\"/target\">link</a></body></html>".to_vec(),
        "text/html",
      )),
      _ => None,
    },
  ) else {
    return;
  };
  let child_url = child_server.url("frame.html");
  let expected_hover_url = child_server.url("target");

  let child_url_for_parent = child_url.clone();
  let Some(parent_server) = TestServer::start(
    "oopif_hovered_url_uses_deepest_frame_parent",
    move |path| match path {
      "/index.html" => Some((
        format!("<!doctype html><html><body><iframe src=\"{child_url_for_parent}\"></iframe></body></html>")
          .into_bytes(),
        "text/html",
      )),
      _ => None,
    },
  ) else {
    let _ = child_server.shutdown_and_join();
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
    width: 100,
    height: 100,
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
    "expected FrameReady for parent frame (err={:?})",
    ready.last_error
  );
  assert!(
    !ready.subframes.is_empty(),
    "expected parent to report at least one subframe (err={:?})",
    ready.last_error
  );
  let iframe = ready.subframes[0].clone();

  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  child_renderer.send(&BrowserToRenderer::Resize {
    frame_id: child_frame,
    width: 50,
    height: 50,
    dpr: 1.0,
  });
  let child_context = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    &child_url,
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    &iframe,
  );
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    navigation: IframeNavigation::Url(child_url.clone()),
    context: child_context,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });
  let _ = child_renderer.recv_frame_ready(Duration::from_secs(10));

  let mut hit_tester = FrameHitTester::new(parent_frame);
  hit_tester.set_frame_size(parent_frame, 100, 100);
  hit_tester.set_frame_size(child_frame, 50, 50);
  hit_tester.set_subframes(parent_frame, vec![iframe]);

  let mut router = HoverRouter::new(parent_frame);
  let mut ui_state = fastrender_ipc::HoverState::default();
  let mut seq: u64 = 1;

  // Hover inside the iframe so we pick up the child's hovered URL.
  let (targets, emit) = router.on_pointer_move(&hit_tester, 10.0, 10.0);
  if let Some(state) = emit {
    ui_state = state;
  }
  let event_seq = seq;
  seq += 1;
  for (frame_id, x_css, y_css) in targets {
    let msg = BrowserToRenderer::PointerMove {
      frame_id,
      x_css,
      y_css,
      seq: event_seq,
    };
    if frame_id == parent_frame {
      parent_renderer.send(&msg);
    } else if frame_id == child_frame {
      child_renderer.send(&msg);
    }
  }
  drive_hover_until(
    &[&parent_renderer, &child_renderer],
    &mut router,
    &mut ui_state,
    CursorKind::Pointer,
  );
  assert_eq!(
    ui_state.hovered_url.as_deref(),
    Some(expected_hover_url.as_str()),
    "expected hovered_url to come from child frame"
  );

  parent_renderer.shutdown();
  child_renderer.shutdown();
  let _ = seq;
  let _ = parent_server.shutdown_and_join();
  let _ = child_server.shutdown_and_join();
}

#[test]
fn oopif_cursor_uses_deepest_frame_in_nested_iframes() {
  let _net_guard = net_test_lock();

  let Some(grandchild_server) = TestServer::start(
    "oopif_cursor_uses_deepest_frame_in_nested_iframes_grandchild",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><html><body><input></body></html>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };
  let grandchild_url = grandchild_server.url("frame.html");

  let grandchild_url_for_child = grandchild_url.clone();
  let Some(child_server) = TestServer::start(
    "oopif_cursor_uses_deepest_frame_in_nested_iframes_child",
    move |path| match path {
      "/frame.html" => Some((
        format!("<!doctype html><html><body><iframe src=\"{grandchild_url_for_child}\"></iframe></body></html>").into_bytes(),
        "text/html",
      )),
      _ => None,
    },
  ) else {
    let _ = grandchild_server.shutdown_and_join();
    return;
  };
  let child_url = child_server.url("frame.html");

  let child_url_for_parent = child_url.clone();
  let Some(parent_server) = TestServer::start(
    "oopif_cursor_uses_deepest_frame_in_nested_iframes_parent",
    move |path| match path {
      "/index.html" => Some((
        format!("<!doctype html><html><body><iframe src=\"{child_url_for_parent}\"></iframe></body></html>").into_bytes(),
        "text/html",
      )),
      _ => None,
    },
  ) else {
    let _ = child_server.shutdown_and_join();
    let _ = grandchild_server.shutdown_and_join();
    return;
  };
  let parent_url = parent_server.url("index.html");

  let site_keys = SiteKeyFactory::new_with_seed(1);
  let parent_site_key = site_keys.site_key_for_navigation(&parent_url, None);

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame { frame_id: parent_frame });
  parent_renderer.send(&BrowserToRenderer::Resize {
    frame_id: parent_frame,
    width: 100,
    height: 100,
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
  parent_renderer.send(&BrowserToRenderer::RequestRepaint { frame_id: parent_frame });

  let parent_ready = parent_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(parent_ready.frame_id, parent_frame);
  assert!(
    !parent_ready.subframes.is_empty(),
    "expected parent to report a child iframe (err={:?})",
    parent_ready.last_error
  );
  let child_info = parent_ready.subframes[0].clone();
  let child_frame = child_info.child;

  let child_site_key = site_keys.site_key_for_navigation(&child_url, Some(&parent_site_key));
  let child_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    &child_url,
    Some(&parent_site_key),
    parent_url.clone(),
    ReferrerPolicy::default(),
    &child_info,
  );

  let mut child_renderer = RendererProc::spawn();
  child_renderer.send(&BrowserToRenderer::CreateFrame { frame_id: child_frame });
  child_renderer.send(&BrowserToRenderer::Resize {
    frame_id: child_frame,
    width: 50,
    height: 50,
    dpr: 1.0,
  });
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    navigation: IframeNavigation::Url(child_url.clone()),
    context: child_ctx,
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint { frame_id: child_frame });

  let child_ready = child_renderer.recv_frame_ready(Duration::from_secs(10));
  assert_eq!(child_ready.frame_id, child_frame);
  assert!(
    !child_ready.subframes.is_empty(),
    "expected child to report a grandchild iframe (err={:?})",
    child_ready.last_error
  );
  let grandchild_info = child_ready.subframes[0].clone();
  let grandchild_frame = grandchild_info.child;

  let grandchild_ctx = NavigationContext::for_subframe_navigation_from_info(
    &site_keys,
    &grandchild_url,
    Some(&child_site_key),
    child_url.clone(),
    ReferrerPolicy::default(),
    &grandchild_info,
  );

  let mut grandchild_renderer = RendererProc::spawn();
  grandchild_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: grandchild_frame,
  });
  grandchild_renderer.send(&BrowserToRenderer::Resize {
    frame_id: grandchild_frame,
    width: 20,
    height: 20,
    dpr: 1.0,
  });
  grandchild_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: grandchild_frame,
    navigation: IframeNavigation::Url(grandchild_url.clone()),
    context: grandchild_ctx,
  });
  grandchild_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: grandchild_frame,
  });
  let _ = grandchild_renderer.recv_frame_ready(Duration::from_secs(10));

  let mut hit_tester = FrameHitTester::new(parent_frame);
  hit_tester.set_frame_size(parent_frame, 100, 100);
  hit_tester.set_frame_size(child_frame, 50, 50);
  hit_tester.set_frame_size(grandchild_frame, 20, 20);
  hit_tester.set_subframes(parent_frame, vec![child_info]);
  hit_tester.set_subframes(child_frame, vec![grandchild_info]);

  let mut router = HoverRouter::new(parent_frame);
  let mut ui_state = fastrender_ipc::HoverState::default();
  let mut seq: u64 = 1;

  // Inside child but outside grandchild => child is deepest => default cursor.
  {
    let (targets, emit) = router.on_pointer_move(&hit_tester, 30.0, 30.0);
    if let Some(state) = emit {
      ui_state = state;
    }
    let event_seq = seq;
    seq += 1;
    for (frame_id, x_css, y_css) in targets {
      let msg = BrowserToRenderer::PointerMove {
        frame_id,
        x_css,
        y_css,
        seq: event_seq,
      };
      if frame_id == parent_frame {
        parent_renderer.send(&msg);
      } else if frame_id == child_frame {
        child_renderer.send(&msg);
      } else {
        panic!("unexpected pointer target frame: {frame_id:?}");
      }
    }
    drive_hover_until(
      &[&parent_renderer, &child_renderer, &grandchild_renderer],
      &mut router,
      &mut ui_state,
      CursorKind::Default,
    );
  }

  // Inside grandchild => grandchild wins => text cursor.
  {
    let (targets, emit) = router.on_pointer_move(&hit_tester, 10.0, 10.0);
    if let Some(state) = emit {
      ui_state = state;
    }
    let event_seq = seq;
    seq += 1;
    for (frame_id, x_css, y_css) in targets {
      let msg = BrowserToRenderer::PointerMove {
        frame_id,
        x_css,
        y_css,
        seq: event_seq,
      };
      if frame_id == parent_frame {
        parent_renderer.send(&msg);
      } else if frame_id == grandchild_frame {
        grandchild_renderer.send(&msg);
      } else {
        panic!("unexpected pointer target frame: {frame_id:?}");
      }
    }
    drive_hover_until(
      &[&parent_renderer, &child_renderer, &grandchild_renderer],
      &mut router,
      &mut ui_state,
      CursorKind::Text,
    );
  }

  parent_renderer.shutdown();
  child_renderer.shutdown();
  grandchild_renderer.shutdown();
  let _ = seq;
  let _ = parent_server.shutdown_and_join();
  let _ = child_server.shutdown_and_join();
  let _ = grandchild_server.shutdown_and_join();
}
