use fastrender::debug::runtime::{self, RuntimeToggles};
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::SvgFilterResolver;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn svg_filter_resolver_caches_parsed_filters_when_enabled() {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_SVG_FILTER_RESOLVER_CACHE_ITEMS".to_string(),
    "8".to_string(),
  )])));

  runtime::with_thread_runtime_toggles(toggles, || {
    let image_cache = ImageCache::new();
    let mut svg_defs = HashMap::new();
    svg_defs.insert(
      "recolor".to_string(),
      r#"<filter id="recolor"><feFlood flood-color="red" flood-opacity="1"/></filter>"#.to_string(),
    );

    let mut resolver =
      SvgFilterResolver::new(Some(Arc::new(svg_defs)), Vec::new(), Some(&image_cache));

    let first = resolver.resolve("#recolor").expect("expected filter reference to resolve");
    let second = resolver
      .resolve("url(#recolor)")
      .expect("expected filter reference to resolve via url(...)");

    assert!(
      Arc::ptr_eq(&first, &second),
      "expected SvgFilterResolver to return cached filter instances"
    );
  });
}

