use fastrender::dom::parse_html;
use fastrender::geometry::Size;
use fastrender::html::image_prefetch::{
  discover_image_prefetch_requests, discover_image_prefetch_urls, ImagePrefetchLimits,
  ImagePrefetchRequest,
};
use fastrender::html::images::ImageSelectionContext;
use fastrender::tree::box_tree::CrossOriginAttribute;
use std::fs;
use std::path::PathBuf;

#[test]
fn image_prefetch_discovers_video_poster_from_wrapper_data_poster_url() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_video_poster_wrapper");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let dom = parse_html(&html).expect("parse DOM");
  let ctx = ImageSelectionContext {
    device_pixel_ratio: 1.0,
    slot_width: None,
    viewport: Some(Size::new(800.0, 600.0)),
    media_context: None,
    font_size: None,
    root_font_size: None,
    base_url: Some("https://example.com/"),
  };

  let urls = discover_image_prefetch_urls(&dom, ctx, ImagePrefetchLimits::default());
  assert_eq!(urls.urls, vec!["https://example.com/red.svg".to_string()]);

  let requests = discover_image_prefetch_requests(&dom, ctx, ImagePrefetchLimits::default());
  assert_eq!(
    requests.requests,
    vec![ImagePrefetchRequest {
      url: "https://example.com/red.svg".to_string(),
      crossorigin: CrossOriginAttribute::None,
    }]
  );
}
