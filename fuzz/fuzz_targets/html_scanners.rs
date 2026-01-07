#![no_main]

use fastrender::html::asset_discovery::discover_html_asset_urls_with_srcset_limit;
use fastrender::html::meta_refresh::{extract_js_location_redirect, extract_meta_refresh_url};
use fastrender::html::strip_template_contents;
use libfuzzer_sys::fuzz_target;

const MAX_LEN: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
  let slice = if data.len() > MAX_LEN { &data[..MAX_LEN] } else { data };
  let html = String::from_utf8_lossy(slice);
  let html = html.as_ref();

  let stripped = strip_template_contents(html);
  assert!(stripped.len() <= html.len());

  let meta_a = extract_meta_refresh_url(html);
  let meta_b = extract_meta_refresh_url(html);
  assert_eq!(meta_a, meta_b);

  let js_a = extract_js_location_redirect(html);
  let js_b = extract_js_location_redirect(html);
  assert_eq!(js_a, js_b);

  let assets_a = discover_html_asset_urls_with_srcset_limit(html, "https://example.com/", 4);
  let assets_b = discover_html_asset_urls_with_srcset_limit(html, "https://example.com/", 4);
  assert_eq!(assets_a, assets_b);
});
