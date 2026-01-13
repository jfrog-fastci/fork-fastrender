#![cfg(not(feature = "direct_network"))]

use std::sync::Arc;

use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, FastRender};

#[derive(Clone, Default)]
struct MockFetcher;

impl ResourceFetcher for MockFetcher {
  fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
    Err(Error::Other("MockFetcher.fetch not implemented".to_string()))
  }
}

#[test]
fn building_without_fetcher_fails_deterministically() {
  let err = FastRender::builder()
    .build()
    .expect_err("expected builder to require an injected ResourceFetcher in networkless builds");
  assert!(
    err.to_string().contains("direct_network"),
    "expected error to mention direct_network; got {err:?}"
  );
}

#[test]
fn building_with_injected_fetcher_succeeds() {
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MockFetcher::default());
  let mut renderer = FastRender::builder()
    .fetcher(fetcher)
    .build()
    .expect("expected renderer to build with injected fetcher");
  // Sanity: the renderer should be usable for non-network renders.
  let pixmap = renderer
    .render_html("<!doctype html><html><body>ok</body></html>", 10, 10)
    .expect("render");
  assert_eq!(pixmap.width(), 10);
  assert_eq!(pixmap.height(), 10);
}
