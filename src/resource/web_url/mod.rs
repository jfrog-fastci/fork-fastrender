//! Fallible, size-bounded URL / URLSearchParams core types.
//!
//! These types are intended for hostile input (JavaScript bindings, arbitrary URL strings).
//! All allocations are guarded (fallible + bounded) to avoid abort-on-OOM.

mod error;
mod limits;
mod search_params;
mod url;

pub use error::{WebUrlError, WebUrlLimitKind, WebUrlSetter};
pub use limits::WebUrlLimits;
pub use search_params::WebUrlSearchParams;
pub use url::WebUrl;

#[cfg(test)]
mod tests {
  use super::{WebUrl, WebUrlError, WebUrlLimitKind, WebUrlLimits, WebUrlSearchParams};

  #[test]
  fn resolves_relative_url_with_base() {
    let limits = WebUrlLimits::default();
    let url = WebUrl::parse("foo", Some("https://example.com/bar/baz"), &limits).unwrap();
    assert_eq!(url.href().unwrap(), "https://example.com/bar/foo");
  }

  #[test]
  fn errors_on_invalid_base_url() {
    let limits = WebUrlLimits::default();
    let err = WebUrl::parse("foo", Some("not a url"), &limits).unwrap_err();
    assert!(matches!(err, WebUrlError::InvalidBase { .. }));
  }

  #[test]
  fn urlsearchparams_preserves_duplicates_and_ordering() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a=1&a=2&b=3", &limits).unwrap();
    assert_eq!(params.get("a").unwrap(), Some("1".to_string()));
    assert_eq!(
      params.get_all("a").unwrap(),
      vec!["1".to_string(), "2".to_string()]
    );
    assert!(params.has("b", None).unwrap());
    assert_eq!(params.serialize().unwrap(), "a=1&a=2&b=3");
  }

  #[test]
  fn urlsearchparams_set_replaces_all_entries_for_name() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a=1&b=2&a=3", &limits).unwrap();
    params.set("a", "9").unwrap();
    assert_eq!(params.serialize().unwrap(), "a=9&b=2");
  }

  #[test]
  fn urlsearchparams_serialization_percent_encodes() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::new(&limits);
    params.append("a", "b ~").unwrap();
    // SPACE becomes '+' and '~' is percent-encoded per the x-www-form-urlencoded encode set.
    assert_eq!(params.serialize().unwrap(), "a=b+%7E");
  }

  #[test]
  fn urlsearchparams_serialization_handles_non_ascii() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::new(&limits);
    params.append("café", "☕").unwrap();
    assert_eq!(params.serialize().unwrap(), "caf%C3%A9=%E2%98%95");
  }

  #[test]
  fn urlsearchparams_is_live_and_updates_href_on_mutation() {
    let limits = WebUrlLimits::default();
    let url = WebUrl::parse("https://example.com/?a=b%20~", None, &limits).unwrap();
    let params = url.search_params();

    // Reading searchParams does not normalize URL.search.
    assert_eq!(url.search().unwrap(), "?a=b%20~");
    assert_eq!(params.get("a").unwrap(), Some("b ~".to_string()));
    assert_eq!(params.serialize().unwrap(), "a=b+%7E");
    assert_eq!(url.search().unwrap(), "?a=b%20~");

    // Mutating searchParams rewrites URL.search using urlencoded serialization.
    params.append("c", "d").unwrap();
    assert_eq!(url.href().unwrap(), "https://example.com/?a=b+%7E&c=d");
    assert_eq!(url.search().unwrap(), "?a=b+%7E&c=d");
  }

  #[test]
  fn url_search_setter_updates_associated_searchparams() {
    let limits = WebUrlLimits::default();
    let url = WebUrl::parse("https://example.com/", None, &limits).unwrap();
    let params = url.search_params();

    url.set_search("?q=a+b").unwrap();
    assert_eq!(url.search().unwrap(), "?q=a+b");
    assert_eq!(params.get("q").unwrap(), Some("a b".to_string()));
    assert_eq!(params.serialize().unwrap(), "q=a+b");

    url.set_search("").unwrap();
    assert_eq!(url.search().unwrap(), "");
    assert!(!params.has("q", None).unwrap());
  }

  #[test]
  fn urlsearchparams_encoding_spaces_and_plus() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a=1+2&b=3%2B4", &limits).unwrap();
    assert_eq!(params.get("a").unwrap(), Some("1 2".to_string()));
    assert_eq!(params.get("b").unwrap(), Some("3+4".to_string()));

    params.set("a", "x y").unwrap();
    params.append("c", "1+2").unwrap();
    assert_eq!(params.serialize().unwrap(), "a=x+y&b=3%2B4&c=1%2B2");
  }

  #[test]
  fn urlsearchparams_live_set_replaces_all_entries_for_name() {
    let limits = WebUrlLimits::default();
    let url = WebUrl::parse("https://example.com/?a=1&b=2&a=3", None, &limits).unwrap();
    let params = url.search_params();
    params.set("a", "9").unwrap();
    assert_eq!(url.href().unwrap(), "https://example.com/?a=9&b=2");
    assert_eq!(params.serialize().unwrap(), "a=9&b=2");
  }

  #[test]
  fn url_hash_getter_and_setter() {
    let limits = WebUrlLimits::default();
    let url = WebUrl::parse("https://example.com/#a", None, &limits).unwrap();
    assert_eq!(url.hash().unwrap(), "#a");
    url.set_hash("#b").unwrap();
    assert_eq!(url.hash().unwrap(), "#b");
    assert_eq!(url.href().unwrap(), "https://example.com/#b");

    url.set_hash("").unwrap();
    assert_eq!(url.hash().unwrap(), "");
    assert_eq!(url.href().unwrap(), "https://example.com/");
  }

  #[test]
  fn limit_exceeded_on_input_bytes() {
    let limits = WebUrlLimits {
      max_input_bytes: 4,
      max_query_pairs: 16,
      max_total_query_bytes: 1024,
    };

    let err = WebUrl::parse("https://example.com", None, &limits).unwrap_err();
    assert!(matches!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: 4,
        ..
      }
    ));

    let err = WebUrl::parse("foo", Some("https://example.com"), &limits).unwrap_err();
    assert!(matches!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: 4,
        ..
      }
    ));
  }

  #[test]
  fn limit_exceeded_on_query_pair_count() {
    let limits = WebUrlLimits {
      max_input_bytes: 1024,
      max_query_pairs: 2,
      max_total_query_bytes: 1024,
    };

    let url = WebUrl::parse("https://example.com/?a=1&b=2&c=3", None, &limits).unwrap();
    let params = url.search_params();
    let err = params.serialize().unwrap_err();
    assert!(matches!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::QueryPairs,
        limit: 2,
        ..
      }
    ));
  }

  #[test]
  fn limit_exceeded_on_total_decoded_query_bytes() {
    let limits = WebUrlLimits {
      max_input_bytes: 1024,
      max_query_pairs: 16,
      max_total_query_bytes: 3,
    };

    let url = WebUrl::parse("https://example.com/?a=1&bb=2", None, &limits).unwrap();
    let params = url.search_params();
    let err = params.has("a", None).unwrap_err();
    assert!(matches!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: 3,
        ..
      }
    ));
  }

  #[test]
  fn limit_exceeded_on_href_output_length() {
    let mut limits = WebUrlLimits::default();
    limits.max_input_bytes = 20;

    let url = WebUrl::parse("https://example.com/", None, &limits).unwrap();
    assert_eq!(url.href().unwrap(), "https://example.com/");

    let err = url.set_pathname("          ").unwrap_err();
    assert!(matches!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: 20,
        ..
      }
    ));
    assert_eq!(url.href().unwrap(), "https://example.com/");
  }
}
