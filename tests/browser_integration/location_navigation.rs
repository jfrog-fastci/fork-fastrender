use fastrender::dom2::NodeId;
use fastrender::js::{
  EventLoop, LocationNavigationRequest, ScriptElementSpec, WindowRealm, WindowRealmConfig,
};
use fastrender::{
  BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, Error, RenderOptions, Result,
};

use super::support::{rgba_at, TempSite};

struct VmJsLocationExecutor {
  realm: Option<WindowRealm>,
  realm_document_url: String,
  pending_navigation: Option<LocationNavigationRequest>,
}

impl VmJsLocationExecutor {
  fn new() -> Result<Self> {
    Ok(Self {
      realm: None,
      realm_document_url: "about:blank".to_string(),
      pending_navigation: None,
    })
  }

  fn ensure_realm(&mut self) -> Result<&mut WindowRealm> {
    if self.realm.is_none() {
      self.realm = Some(
        WindowRealm::new(WindowRealmConfig::new(self.realm_document_url.clone()))
          .map_err(|err| Error::Other(err.to_string()))?,
      );
    }
    Ok(self.realm.as_mut().expect("realm should be initialized"))
  }
}

impl BrowserTabJsExecutor for VmJsLocationExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let realm = self.ensure_realm()?;
    realm.set_base_url(spec.base_url.clone());

    match realm.exec_script(script_text) {
      Ok(_) => Ok(()),
      Err(err) => {
        if let Some(req) = realm.take_pending_navigation_request() {
          // Clear the interrupt flag so the realm can be reused if the embedding chooses to keep
          // executing (e.g. navigation fails and scripts continue running).
          realm.reset_interrupt();
          self.pending_navigation = Some(req);
          return Ok(());
        }
        Err(Error::Other(err.to_string()))
      }
    }
  }

  fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
    self.pending_navigation.take()
  }

  fn on_navigation_committed(&mut self, document_url: Option<&str>) {
    self.realm_document_url = document_url.unwrap_or("about:blank").to_string();
    self.realm = None;
    self.pending_navigation = None;
  }
}

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
  let executor = VmJsLocationExecutor::new()?;
  let mut tab = BrowserTab::from_html("", options.clone(), executor)?;

  tab.navigate_to_url(&page1_url, options.clone())?;
  let pixmap = tab.render_frame()?;

  assert_eq!(rgba_at(&pixmap, 32, 32), [0, 0, 255, 255]);
  Ok(())
}

