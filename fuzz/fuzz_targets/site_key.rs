#![no_main]

use fastrender::site_isolation::SiteKeyFactory;
use fastrender::ui::SiteKey as UiSiteKey;
use libfuzzer_sys::fuzz_target;

/// Keep inputs small so libFuzzer explores more cases (and so URL parsing doesn't allocate
/// quadratic amounts of memory for very long strings).
const MAX_URL_LEN: usize = 8 * 1024;

fuzz_target!(|data: &[u8]| {
  let data = if data.len() > MAX_URL_LEN {
    &data[..MAX_URL_LEN]
  } else {
    data
  };
  let input = String::from_utf8_lossy(data);
  let input = input.as_ref();

  // Strict parsing used in some browser-side layers.
  let _ = UiSiteKey::from_url(input);

  // Site isolation key derivation (including parent inheritance for special URLs like
  // `about:blank`).
  let factory = SiteKeyFactory::new_with_seed(1);
  let current = factory.site_key_for_navigation("https://example.com", None, false);
  let next = factory.site_key_for_navigation(input, Some(&current), false);
  let _ = next != current;
});
