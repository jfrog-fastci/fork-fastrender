use fastrender::resource::ResourceFetcher;
use js_wpt_dom_runner::WptResourceFetcher;
use std::path::PathBuf;

fn corpus_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/wpt_dom")
}

#[test]
fn existing_corpus_file_fetches() {
  let fetcher = WptResourceFetcher::new(corpus_root()).expect("create fetcher");
  let res = fetcher
    .fetch("https://web-platform.test/resources/testharness.js")
    .expect("fetch resource");
  assert_eq!(res.status, Some(200));
  assert!(!res.bytes.is_empty());
  assert_eq!(res.content_type.as_deref(), Some("application/javascript"));
}

#[test]
fn missing_file_returns_404_without_error() {
  let fetcher = WptResourceFetcher::new(corpus_root()).expect("create fetcher");
  let res = fetcher
    .fetch("https://web-platform.test/resources/definitely_missing_file.js")
    .expect("fetch missing resource");
  assert_eq!(res.status, Some(404));
  assert!(res.bytes.is_empty());
}

#[test]
fn non_wpt_origin_is_rejected_offline() {
  let fetcher = WptResourceFetcher::new(corpus_root()).expect("create fetcher");
  let err = fetcher
    .fetch("https://example.com/resources/testharness.js")
    .expect_err("non-WPT origin should error");
  let msg = err.to_string();
  assert!(
    msg.contains("offline WPT fetcher blocked"),
    "unexpected error: {msg}"
  );
}

#[test]
fn cookie_round_trip_is_deterministic() {
  let fetcher = WptResourceFetcher::new(corpus_root()).expect("create fetcher");
  assert_eq!(
    fetcher.cookie_header_value("https://web-platform.test/"),
    Some(String::new())
  );

  fetcher.store_cookie_from_document("https://web-platform.test/", "b=c; Path=/");
  fetcher.store_cookie_from_document("https://web-platform.test/", "a=b");

  assert_eq!(
    fetcher.cookie_header_value("https://web-platform.test/"),
    Some("a=b; b=c".to_string())
  );
}

#[test]
fn path_traversal_is_rejected_even_when_target_missing() {
  let fetcher = WptResourceFetcher::new(corpus_root()).expect("create fetcher");
  let err = fetcher
    .fetch("https://web-platform.test/resources/../secrets.txt")
    .expect_err("path traversal should error");
  let msg = err.to_string();
  assert!(
    msg.contains("invalid WPT corpus path"),
    "unexpected error: {msg}"
  );
}

#[test]
fn http_scheme_is_accepted() {
  let fetcher = WptResourceFetcher::new(corpus_root()).expect("create fetcher");
  let res = fetcher
    .fetch("http://web-platform.test/resources/testharness.js")
    .expect("fetch resource over http");
  assert_eq!(res.status, Some(200));
}
