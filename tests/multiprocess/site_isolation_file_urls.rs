use fastrender::site_isolation::{FileUrlSiteIsolation, SiteKey, SiteKeyFactory};
use std::collections::HashMap;
use url::Url;

#[derive(Default)]
struct FakeProcessRegistry {
  next_id: u64,
  by_site: HashMap<SiteKey, u64>,
}

impl FakeProcessRegistry {
  fn new() -> Self {
    Self {
      next_id: 1,
      by_site: HashMap::new(),
    }
  }

  fn open_tab(&mut self, factory: &SiteKeyFactory, url: &Url) -> u64 {
    let site = factory.site_key_for_navigation(url.as_str(), None);
    if let Some(existing) = self.by_site.get(&site) {
      return *existing;
    }
    let id = self.next_id;
    self.next_id += 1;
    self.by_site.insert(site, id);
    id
  }
}

#[test]
fn file_tabs_do_not_share_renderer_process_when_file_isolation_enabled() {
  let dir = tempfile::tempdir().expect("temp dir");
  let a = dir.path().join("a.html");
  let b = dir.path().join("b.html");
  std::fs::write(&a, "<!doctype html><p>a</p>").unwrap();
  std::fs::write(&b, "<!doctype html><p>b</p>").unwrap();

  let url_a = Url::from_file_path(&a).unwrap();
  let url_b = Url::from_file_path(&b).unwrap();

  let factory = SiteKeyFactory::new_with_seed_and_file_url_isolation(
    1,
    FileUrlSiteIsolation::OpaquePerUrl,
  );
  let mut registry = FakeProcessRegistry::new();

  let proc_a = registry.open_tab(&factory, &url_a);
  let proc_b = registry.open_tab(&factory, &url_b);

  assert_ne!(
    proc_a, proc_b,
    "different file:// pages must not reuse the same renderer process when file isolation is enabled"
  );
}

