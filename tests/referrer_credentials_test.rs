use fastrender::resource::{FetchDestination, FetchRequest, HttpFetcher, ReferrerPolicy, ResourceFetcher};

#[test]
fn referer_header_strips_url_credentials() {
  // The `Referer` header should never include embedded URL credentials (`user:pass@`).
  // Ensure browser-like headers are enabled for deterministic header generation.
  std::env::set_var("FASTR_HTTP_BROWSER_HEADERS", "1");

  let fetcher = HttpFetcher::new();
  for referrer_url in [
    // Strict URL parsing succeeds.
    "https://user:pass@example.com/path/page.html?q=1#frag",
    // Tolerant path (invalid `|` in query) should still strip credentials.
    "https://user:pass@example.com/path/page.html?q=|#frag",
  ] {
    let req = FetchRequest::new("https://example.com/img.png", FetchDestination::Image)
      .with_referrer_url(referrer_url)
      .with_referrer_policy(ReferrerPolicy::NoReferrerWhenDowngrade);

    let referer = fetcher
      .request_header_value(req, "Referer")
      .expect("HttpFetcher should deterministically construct Referer");

    let expected = if referrer_url.contains("q=|") {
      "https://example.com/path/page.html?q=|"
    } else {
      "https://example.com/path/page.html?q=1"
    };
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
