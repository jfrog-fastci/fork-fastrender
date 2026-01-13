use fastrender::js::{RunLimits, RunUntilIdleOutcome};
use fastrender::scroll::ScrollState;
use fastrender::{BrowserTab, Point, RenderOptions, Result, VmJsBrowserTabExecutor};

fn element_id_from_point(tab: &mut BrowserTab, x: i32, y: i32) -> Result<String> {
  let script = format!(
    "var el = document.elementFromPoint({x}, {y});\n\
     document.documentElement.setAttribute('data-hit', el ? el.id : 'null');"
  );
  let body = tab.dom().body().expect("<body> element");
  {
    let dom = tab.dom_mut();
    let script_node = dom.create_element("script", "");
    let text_node = dom.create_text(&script);
    dom
      .append_child(script_node, text_node)
      .expect("append script text node");
    dom
      .append_child(body, script_node)
      .expect("append script element into <body>");
  }

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle,
    "expected script execution to run to idle"
  );

  let html = tab.dom().document_element().expect("documentElement");
  Ok(
    tab
      .dom()
      .get_attribute(html, "data-hit")
      .expect("read data-hit attribute")
      .expect("data-hit attribute should be set")
      .to_string(),
  )
}

#[test]
fn browser_tab_set_scroll_state_updates_dom2_element_from_point_hit_testing() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          body { height: 2000px; }
          #top, #bottom {
            position: absolute;
            left: 0;
            width: 200px;
            height: 50px;
          }
          #top { top: 0; background: red; }
          #bottom { top: 1000px; background: blue; }
        </style>
      </head>
      <body>
        <div id="top"></div>
        <div id="bottom"></div>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(200, 50);
  let mut tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::default())?;

  assert_eq!(element_id_from_point(&mut tab, 10, 10)?, "top");

  tab.set_scroll_state(ScrollState::with_viewport(Point::new(0.0, 1000.0)));
  assert_eq!(tab.scroll_state().viewport.y, 1000.0);
  assert_eq!(element_id_from_point(&mut tab, 10, 10)?, "bottom");
  Ok(())
}

#[test]
fn browser_tab_set_viewport_and_dpr_update_dom2_element_from_point_hit_testing() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #low, #high {
            position: absolute;
            left: 0;
            top: 0;
            width: 50px;
            height: 50px;
          }
          #low { background: red; }
          #high { display: none; background: blue; }
          @media (min-resolution: 2dppx) {
            #low { display: none; }
            #high { display: block; }
          }
        </style>
      </head>
      <body>
        <div id="low"></div>
        <div id="high"></div>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(100, 100);
  let mut tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::default())?;

  assert_eq!(tab.viewport_size_css(), Some((100, 100)));

  tab.set_device_pixel_ratio(1.0);
  assert_eq!(element_id_from_point(&mut tab, 10, 10)?, "low");

  tab.set_device_pixel_ratio(2.0);
  assert_eq!(element_id_from_point(&mut tab, 10, 10)?, "high");

  // Shrink the viewport and ensure the same point is now out-of-bounds for hit testing.
  tab.set_viewport(5, 5);
  assert_eq!(tab.viewport_size_css(), Some((5, 5)));
  assert_eq!(element_id_from_point(&mut tab, 10, 10)?, "null");
  Ok(())
}
