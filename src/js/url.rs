//! Spec-shaped core URL + URLSearchParams primitives.
//!
//! This module is intended to back forthcoming WebIDL JS bindings for WHATWG `URL` and
//! `URLSearchParams`. It is not an embedding layer; it provides deterministic parsing and
//! serialization built on the `url` crate, and a small `URLSearchParams` list implementation that
//! preserves duplicates and stable ordering.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum UrlError {
  #[error("invalid base URL {base:?}: {source}")]
  InvalidBase {
    base: String,
    #[source]
    source: ::url::ParseError,
  },
  #[error("failed to parse URL {input:?}: {source}")]
  Parse {
    input: String,
    #[source]
    source: ::url::ParseError,
  },
  #[error("failed to set href to {value:?}: {source}")]
  SetterFailure {
    value: String,
    #[source]
    source: ::url::ParseError,
  },
}

/// A WHATWG-shaped URL wrapper used by JS bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
  inner: ::url::Url,
}

impl Url {
  /// Parse an input string into a URL.
  ///
  /// When `base` is provided it is first parsed as a URL; if it is invalid this returns
  /// `UrlError::InvalidBase`. Otherwise `input` is parsed using the WHATWG URL rules with the parsed
  /// base.
  pub fn parse(input: &str, base: Option<&str>) -> Result<Self, UrlError> {
    let inner = match base {
      Some(base) => {
        let base_url = ::url::Url::parse(base).map_err(|source| UrlError::InvalidBase {
          base: base.to_string(),
          source,
        })?;
        ::url::Url::options()
          .base_url(Some(&base_url))
          .parse(input)
          .map_err(|source| UrlError::Parse {
            input: input.to_string(),
            source,
          })?
      }
      None => ::url::Url::parse(input).map_err(|source| UrlError::Parse {
        input: input.to_string(),
        source,
      })?,
    };
    Ok(Self { inner })
  }

  /// Equivalent to the WHATWG `URL.href` getter.
  pub fn href(&self) -> String {
    self.inner.as_str().to_string()
  }

  /// Equivalent to the WHATWG `URL.href` setter.
  pub fn set_href(&mut self, value: &str) -> Result<(), UrlError> {
    let parsed = ::url::Url::parse(value).map_err(|source| UrlError::SetterFailure {
      value: value.to_string(),
      source,
    })?;
    self.inner = parsed;
    Ok(())
  }

  /// Equivalent to the WHATWG `URL.origin` getter.
  pub fn origin(&self) -> String {
    self.inner.origin().ascii_serialization()
  }

  /// Return the (raw) query string without the leading `?`, if present.
  pub fn query(&self) -> Option<String> {
    self.inner.query().map(str::to_string)
  }

  /// Set the (raw) query string without the leading `?`.
  pub fn set_query(&mut self, query: Option<&str>) {
    self.inner.set_query(query);
  }

  /// Parse this URL's query using the `application/x-www-form-urlencoded` parser.
  pub fn search_params(&self) -> UrlSearchParams {
    UrlSearchParams::parse(self.inner.query().unwrap_or(""))
  }

  /// Replace this URL's query with the serialization of `params`.
  pub fn set_search_params(&mut self, params: &UrlSearchParams) {
    let serialized = params.to_string();
    if serialized.is_empty() {
      self.inner.set_query(None);
    } else {
      self.inner.set_query(Some(serialized.as_str()));
    }
  }
}

impl std::fmt::Display for Url {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.inner.as_str())
  }
}

/// A WHATWG-shaped URLSearchParams list that preserves duplicates and stable ordering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UrlSearchParams {
  pairs: Vec<(String, String)>,
}

impl UrlSearchParams {
  /// Parse a raw query string such as `"a=b&c=d"` or a leading-`?` variant such as `"?a=b"`.
  pub fn parse(input: &str) -> Self {
    let input = input.strip_prefix('?').unwrap_or(input);
    let pairs = ::url::form_urlencoded::parse(input.as_bytes())
      .map(|(name, value)| (name.into_owned(), value.into_owned()))
      .collect();
    Self { pairs }
  }

  pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
    self.pairs.push((name.into(), value.into()));
  }

  pub fn delete(&mut self, name: &str) {
    self.pairs.retain(|(n, _)| n != name);
  }

  pub fn get(&self, name: &str) -> Option<String> {
    self
      .pairs
      .iter()
      .find_map(|(n, v)| if n == name { Some(v.clone()) } else { None })
  }

  pub fn get_all(&self, name: &str) -> Vec<String> {
    self
      .pairs
      .iter()
      .filter_map(|(n, v)| if n == name { Some(v.clone()) } else { None })
      .collect()
  }

  pub fn has(&self, name: &str) -> bool {
    self.pairs.iter().any(|(n, _)| n == name)
  }

  /// Set the first matching pair's value and remove any remaining pairs with the same name.
  ///
  /// If no existing pair matches `name`, append a new pair to the end of the list.
  pub fn set(&mut self, name: &str, value: impl Into<String>) {
    let value = value.into();
    let mut found = false;
    self.pairs.retain_mut(|(n, v)| {
      if n == name {
        if found {
          false
        } else {
          *v = value.clone();
          found = true;
          true
        }
      } else {
        true
      }
    });
    if !found {
      self.pairs.push((name.to_string(), value));
    }
  }

  fn serialize(&self) -> String {
    let mut serializer = ::url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in &self.pairs {
      serializer.append_pair(name, value);
    }
    serializer.finish()
  }
}

impl std::fmt::Display for UrlSearchParams {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.serialize().as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::{Url, UrlError, UrlSearchParams};

  #[test]
  fn resolves_relative_url_with_base() {
    let url = Url::parse("foo", Some("https://example.com/bar/baz")).unwrap();
    assert_eq!(url.href(), "https://example.com/bar/foo");
  }

  #[test]
  fn errors_on_invalid_base_url() {
    let err = Url::parse("foo", Some("not a url")).unwrap_err();
    assert!(matches!(err, UrlError::InvalidBase { .. }));
  }

  #[test]
  fn urlsearchparams_preserves_duplicates_and_ordering() {
    let params = UrlSearchParams::parse("a=1&a=2&b=3");
    assert_eq!(params.get("a"), Some("1".to_string()));
    assert_eq!(params.get_all("a"), vec!["1".to_string(), "2".to_string()]);
    assert!(params.has("b"));
    assert_eq!(params.to_string(), "a=1&a=2&b=3");
  }

  #[test]
  fn urlsearchparams_set_replaces_all_entries_for_name() {
    let mut params = UrlSearchParams::parse("a=1&b=2&a=3");
    params.set("a", "9");
    assert_eq!(params.to_string(), "a=9&b=2");
  }

  #[test]
  fn urlsearchparams_serialization_percent_encodes() {
    let mut params = UrlSearchParams::default();
    params.append("a", "b ~");
    // SPACE becomes '+' and '~' is percent-encoded per the x-www-form-urlencoded encode set.
    assert_eq!(params.to_string(), "a=b+%7E");
  }

  #[test]
  fn urlsearchparams_serialization_handles_non_ascii() {
    let mut params = UrlSearchParams::default();
    params.append("café", "☕");
    assert_eq!(params.to_string(), "caf%C3%A9=%E2%98%95");
  }

  #[test]
  fn url_and_urlsearchparams_roundtrip_query_serialization() {
    let mut url = Url::parse("https://example.com/?a=b%20~", None).unwrap();
    let params = url.search_params();
    assert_eq!(params.get("a"), Some("b ~".to_string()));
    // URLSearchParams uses application/x-www-form-urlencoded, which differs from URL query encoding.
    assert_eq!(params.to_string(), "a=b+%7E");

    url.set_search_params(&params);
    assert_eq!(url.href(), "https://example.com/?a=b+%7E");
  }
}

