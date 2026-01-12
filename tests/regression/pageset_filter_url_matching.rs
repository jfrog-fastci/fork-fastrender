use fastrender::pageset::{PagesetEntry, PagesetFilter};

#[test]
fn pageset_filter_matches_exact_url_over_stem_when_collisions_exist() {
  let filter = PagesetFilter::from_inputs(&[String::from("HTTPS://EXAMPLE.COM/#frag")])
    .expect("URL inputs should produce a filter");

  let https_entry = PagesetEntry {
    url: "https://example.com".to_string(),
    stem: "example.com".to_string(),
    cache_stem: "example.com--deadbeef".to_string(),
  };
  let http_entry = PagesetEntry {
    url: "http://example.com/".to_string(),
    stem: "example.com".to_string(),
    cache_stem: "example.com--c0ffee00".to_string(),
  };

  assert!(
    filter.matches_entry(&https_entry),
    "expected URL selector to match the exact URL entry"
  );
  assert!(
    !filter.matches_entry(&http_entry),
    "expected URL selector not to match other entries that share the canonical stem"
  );

  assert!(
    filter.unmatched(&[https_entry]).is_empty(),
    "selected URL entry should satisfy the filter"
  );
}

#[test]
fn pageset_filter_url_canonicalization_strips_www_and_trailing_dot() {
  let filter = PagesetFilter::from_inputs(&[String::from("https://WWW.EXAMPLE.com./path#frag")])
    .expect("filter");

  let https_entry = PagesetEntry {
    url: "https://example.com/path".to_string(),
    stem: "example.com_path".to_string(),
    cache_stem: "example.com_path--deadbeef".to_string(),
  };
  let http_entry = PagesetEntry {
    url: "http://example.com/path".to_string(),
    stem: "example.com_path".to_string(),
    cache_stem: "example.com_path--c0ffee00".to_string(),
  };

  assert!(filter.matches_entry(&https_entry));
  assert!(
    !filter.matches_entry(&http_entry),
    "URL selectors should continue to disambiguate schemes even after host canonicalization"
  );
  assert!(filter.unmatched(&[https_entry]).is_empty());
}
