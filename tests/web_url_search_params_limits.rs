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
  let ok_limits = WebUrlLimits {
    max_input_bytes: 1024,
    max_query_pairs: 8,
    max_total_query_bytes: 1024,
  };

  let mut url = WebUrl::parse("https://example.com/?ok=1", None, &ok_limits).unwrap();

  let failing_limits = WebUrlLimits {
    max_input_bytes: 1024,
    max_query_pairs: 2,
    max_total_query_bytes: 1024,
  };
  assert!(url.set_search("a=b&a=b&a=b", &failing_limits).is_err());
  assert_eq!(url.query(), Some("ok=1"));
}
