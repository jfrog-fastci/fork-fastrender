use fastrender::resource::web_url::{
  WebUrl, WebUrlError, WebUrlLimitKind, WebUrlLimits, WebUrlSearchParams,
};

#[test]
fn web_url_search_params_rejects_too_many_pairs() {
  let limits = WebUrlLimits {
    max_input_bytes: 1024,
    max_query_pairs: 3,
    max_total_query_bytes: 1024,
  };

  let input = "a=b&a=b&a=b&a=b&";
  let err = WebUrlSearchParams::parse(input, &limits).unwrap_err();
  assert!(matches!(
    err,
    WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      ..
    }
  ));
}

#[test]
fn web_url_search_params_rejects_oversized_name_or_value() {
  let limits = WebUrlLimits {
    max_input_bytes: 1024,
    max_query_pairs: 8,
    max_total_query_bytes: 10,
  };

  let long_name = "a".repeat(32);
  let input = format!("{long_name}=b");
  let err = WebUrlSearchParams::parse(&input, &limits).unwrap_err();
  assert!(matches!(
    err,
    WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      ..
    }
  ));
}

#[test]
fn web_url_search_params_failure_does_not_mutate_url_query() {
  let failing_limits = WebUrlLimits {
    max_input_bytes: 1024,
    max_query_pairs: 2,
    max_total_query_bytes: 1024,
  };

  // `WebUrl` stores its limits at construction time, so we build the URL with a limit of 2 total
  // query pairs (including the existing query). Exceeding the pair limit via the associated
  // `URLSearchParams` view must not mutate the URL.
  let url = WebUrl::parse("https://example.com/?ok=1&a=b", None, &failing_limits).unwrap();
  let before = url.search().unwrap();

  let params = url.search_params();
  assert!(params.append("c", "d").is_err());
  assert_eq!(url.search().unwrap(), before);
}
