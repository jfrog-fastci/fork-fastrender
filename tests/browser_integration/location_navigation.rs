use fastrender::api::VmJsBrowserTabExecutor;
use fastrender::js::RunLimits;
use fastrender::{BrowserTab, RenderOptions, Result};

use super::support::{rgba_at, TempSite};

#[test]
fn location_href_navigates_to_new_document() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let _page2_url = site.write(
    "page2.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #box { width: 64px; height: 64px; background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  );
  let page1_url = site.write(
    "page1.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="box"></div>
          <script>
            location.href = "page2.html";
          </script>
        </body>
      </html>"#,
  );

  let options = RenderOptions::new().with_viewport(64, 64);
  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html("", options.clone(), executor)?;

  tab.navigate_to_url(&page1_url, options.clone())?;
  let pixmap = tab.render_frame()?;

  assert_eq!(rgba_at(&pixmap, 32, 32), [0, 0, 255, 255]);
  Ok(())
}

#[test]
fn location_href_navigates_from_deferred_script_task() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let _page2_url = site.write(
    "page2.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #box { width: 64px; height: 64px; background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  );
  let _nav_script_url = site.write(
    "nav.js",
    r#"location.href = "page2.html";"#,
  );
  let page1_url = site.write(
    "page1.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="box"></div>
          <script defer src="nav.js"></script>
        </body>
      </html>"#,
  );

  let options = RenderOptions::new().with_viewport(64, 64);
  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html("", options.clone(), executor)?;

  tab.navigate_to_url(&page1_url, options.clone())?;
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;
  let pixmap = tab.render_frame()?;

  assert_eq!(rgba_at(&pixmap, 32, 32), [0, 0, 255, 255]);
  Ok(())
}
