#![no_main]

use fastrender::image_loader::{ImageCache, ImageCacheConfig};
use fastrender::resource::{FetchRequest, FetchedResource, ResourceFetcher};
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

const MAX_INPUT_BYTES: usize = 256 * 1024;

#[derive(Clone)]
struct InMemoryFetcher {
  bytes: Arc<Vec<u8>>,
  content_type: Option<String>,
}

impl ResourceFetcher for InMemoryFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    let mut res = FetchedResource::new((*self.bytes).clone(), self.content_type.clone());
    res.final_url = Some(url.to_string());
    Ok(res)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
    self.fetch(req.url)
  }
}

fuzz_target!(|data: &[u8]| {
  let data = if data.len() > MAX_INPUT_BYTES {
    &data[..MAX_INPUT_BYTES]
  } else {
    data
  };

  // Keep decode budgets tight to avoid OOM while still exercising the decode pipeline.
  let config = ImageCacheConfig {
    max_decoded_pixels: 1024 * 1024,
    max_decoded_dimension: 2048,
    max_cached_images: 16,
    max_cached_image_bytes: 16 * 1024 * 1024,
    max_cached_svg_pixmaps: 16,
    max_cached_svg_bytes: 8 * 1024 * 1024,
    max_cached_raster_pixmaps: 16,
    max_cached_raster_bytes: 16 * 1024 * 1024,
  };

  let fetcher = Arc::new(InMemoryFetcher {
    bytes: Arc::new(data.to_vec()),
    content_type: None,
  });
  let cache = ImageCache::with_fetcher_and_config(fetcher, config);
  let url = "test://fuzz/image";

  let _ = cache.probe(url);
  let _ = cache.load(url);
});

