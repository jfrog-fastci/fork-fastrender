use fastrender_ipc::{FrameHitTester, FrameId};

#[test]
fn pointer_events_none_iframe_does_not_capture_hits() {
  let root = FrameId(1);
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          iframe { pointer-events: none; }
        </style>
      </head>
      <body>
        <!-- Link underneath the iframe (click-through target). -->
        <a href="https://root.test/navigated" id="under">under</a>
        <!-- Cross-origin iframe on top (should be ignored due to pointer-events:none). -->
        <iframe src="https://child.test/"></iframe>
      </body>
    </html>
  "#;

  let subframes = fastrender_renderer::subframes_from_html(root, html);
  assert_eq!(subframes.len(), 1, "expected one iframe");
  assert!(
    !subframes[0].hit_testable,
    "expected iframe to be marked non-hit-testable due to pointer-events:none"
  );

  let child = subframes[0].child;

  let mut tester = FrameHitTester::new(root);
  tester.set_frame_size(root, 100, 100);
  tester.set_frame_size(child, 100, 100);
  tester.set_subframes(root, subframes);

  // Click through the iframe: should target the root frame.
  assert_eq!(tester.hit_test(10.0, 10.0), root);
}

#[test]
fn default_iframe_is_hit_testable_and_wins_hit_testing() {
  let root = FrameId(1);
  let html = r#"
    <!doctype html>
    <html>
      <body>
        <a href="https://root.test/navigated" id="under">under</a>
        <iframe src="https://child.test/"></iframe>
      </body>
    </html>
  "#;

  let subframes = fastrender_renderer::subframes_from_html(root, html);
  assert_eq!(subframes.len(), 1, "expected one iframe");
  assert!(
    subframes[0].hit_testable,
    "expected iframe to be hit-testable by default"
  );

  let child = subframes[0].child;

  let mut tester = FrameHitTester::new(root);
  tester.set_frame_size(root, 100, 100);
  tester.set_frame_size(child, 100, 100);
  tester.set_subframes(root, subframes);

  // Iframe is assumed to cover the full viewport in the placeholder renderer; it should win.
  assert_eq!(tester.hit_test(10.0, 10.0), child);
}

