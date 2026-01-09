use fastrender::resource::{FetchDestination, FetchRequest, HttpFetcher, ReferrerPolicy, ResourceFetcher};
use std::ffi::OsString;

struct EnvVarGuard {
  key: &'static str,
  previous: Option<OsString>,
}

impl EnvVarGuard {
  fn set(key: &'static str, value: &str) -> Self {
    let previous = std::env::var_os(key);
    std::env::set_var(key, value);
    Self { key, previous }
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    match self.previous.take() {
      Some(value) => std::env::set_var(self.key, value),
      None => std::env::remove_var(self.key),
    }
  }
}

#[test]
fn referer_header_strips_url_credentials() {
  let _lock = super::global_test_lock();
  // The `Referer` header should never include embedded URL credentials (`user:pass@`).
  // Ensure browser-like headers are enabled for deterministic header generation.
  let _guard = EnvVarGuard::set("FASTR_HTTP_BROWSER_HEADERS", "1");

  let fetcher = HttpFetcher::new();
  for (referrer_url, expected) in [
    (
      // Strict URL parsing succeeds.
      "https://user:pass@example.com/path/page.html?q=1#frag",
      "https://example.com/path/page.html?q=1",
    ),
    (
      // Tolerant path (invalid `|` in query) should still strip credentials.
      "https://user:pass@example.com/path/page.html?q=|#frag",
      "https://example.com/path/page.html?q=|",
    ),
    (
      // Tolerant path should also normalize scheme/host case and drop default ports.
      "HTTPS://user:pass@EXAMPLE.COM:443/path/page.html?q=|#frag",
      "https://example.com/path/page.html?q=|",
    ),
    (
      // Same for HTTP default port.
      "HTTP://user:pass@EXAMPLE.COM:80/path/page.html?q=|#frag",
      "http://example.com/path/page.html?q=|",
    ),
  ] {
    let req = FetchRequest::new("https://example.com/img.png", FetchDestination::Image)
      .with_referrer_url(referrer_url)
      .with_referrer_policy(ReferrerPolicy::NoReferrerWhenDowngrade);

    let referer = fetcher
      .request_header_value(req, "Referer")
      .expect("HttpFetcher should deterministically construct Referer");

    assert_eq!(referer, expected);
  }

  // Referrer URLs that contain raw control characters must not be reflected into headers.
  let req = FetchRequest::new("https://example.com/img.png", FetchDestination::Image)
    .with_referrer_url("https://user:pass@example.com/path/page.html?q=1\r\nInjected: x")
    .with_referrer_policy(ReferrerPolicy::NoReferrerWhenDowngrade);
  let referer = fetcher
    .request_header_value(req, "Referer")
    .expect("HttpFetcher should deterministically construct Referer");
  assert_eq!(
    referer, "",
    "expected control characters to suppress the Referer header"
  );
}
