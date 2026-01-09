//! Spec-shaped core URL + URLSearchParams primitives.
//!
//! This module is intended to back forthcoming WebIDL JS bindings for WHATWG `URL` and
//! `URLSearchParams`. It is not an embedding layer; it provides deterministic parsing and
//! serialization built on the `url` crate, plus a small `URLSearchParams` list implementation that
//! preserves duplicates and stable ordering.
//!
//! # Live `searchParams`
//!
//! `Url::search_params()` returns a [`UrlSearchParams`] view that stays in sync with the URL's
//! query string:
//! - Mutating the `UrlSearchParams` updates the parent URL's query string.
//! - Setting `Url::search` updates what the associated `UrlSearchParams` sees.
//!
//! This is implemented by sharing an `Rc<RefCell<UrlInner>>` between `Url` and `UrlSearchParams`.

use std::cell::RefCell;
use std::rc::Rc;

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

#[derive(Debug)]
struct UrlInner {
  url: ::url::Url,
}

/// A WHATWG-shaped URL wrapper used by JS bindings.
#[derive(Debug, Clone)]
pub struct Url {
  inner: Rc<RefCell<UrlInner>>,
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

    Ok(Self {
      inner: Rc::new(RefCell::new(UrlInner { url: inner })),
    })
  }

  /// Equivalent to the WHATWG `URL.href` getter.
  pub fn href(&self) -> String {
    self.inner.borrow().url.as_str().to_string()
  }

  /// Equivalent to the WHATWG `URL.href` setter.
  pub fn set_href(&self, value: &str) -> Result<(), UrlError> {
    let parsed = ::url::Url::parse(value).map_err(|source| UrlError::SetterFailure {
      value: value.to_string(),
      source,
    })?;
    self.inner.borrow_mut().url = parsed;
    Ok(())
  }

  /// Equivalent to the WHATWG `URL.origin` getter.
  pub fn origin(&self) -> String {
    self.inner.borrow().url.origin().ascii_serialization()
  }

  /// Equivalent to the WHATWG `URL.protocol` getter.
  pub fn protocol(&self) -> String {
    format!("{}:", self.inner.borrow().url.scheme())
  }

  /// Equivalent to the WHATWG `URL.host` getter.
  pub fn host(&self) -> String {
    let inner = self.inner.borrow();
    let Some(host) = inner.url.host() else {
      return String::new();
    };
    let host = host.to_string();
    match inner.url.port() {
      Some(port) => format!("{host}:{port}"),
      None => host,
    }
  }

  /// Equivalent to the WHATWG `URL.hostname` getter.
  pub fn hostname(&self) -> String {
    self
      .inner
      .borrow()
      .url
      .host()
      .map(|h| h.to_string())
      .unwrap_or_default()
  }

  /// Equivalent to the WHATWG `URL.port` getter.
  pub fn port(&self) -> String {
    self
      .inner
      .borrow()
      .url
      .port()
      .map(|p| p.to_string())
      .unwrap_or_default()
  }

  /// Equivalent to the WHATWG `URL.pathname` getter.
  pub fn pathname(&self) -> String {
    self.inner.borrow().url.path().to_string()
  }

  /// Equivalent to the WHATWG `URL.search` getter.
  pub fn search(&self) -> String {
    self
      .inner
      .borrow()
      .url
      .query()
      .map(|q| format!("?{q}"))
      .unwrap_or_default()
  }

  /// Equivalent to the WHATWG `URL.search` setter.
  ///
  /// - `""` clears the query.
  /// - Otherwise a leading `?` is stripped.
  pub fn set_search(&self, value: &str) {
    let mut inner = self.inner.borrow_mut();
    if value.is_empty() {
      inner.url.set_query(None);
      return;
    }
    let query = value.strip_prefix('?').unwrap_or(value);
    inner.url.set_query(Some(query));
  }

  /// Equivalent to the WHATWG `URL.hash` getter.
  pub fn hash(&self) -> String {
    self
      .inner
      .borrow()
      .url
      .fragment()
      .map(|f| format!("#{f}"))
      .unwrap_or_default()
  }

  /// Equivalent to the WHATWG `URL.hash` setter.
  ///
  /// - `""` clears the fragment.
  /// - Otherwise a leading `#` is stripped.
  pub fn set_hash(&self, value: &str) {
    let mut inner = self.inner.borrow_mut();
    if value.is_empty() {
      inner.url.set_fragment(None);
      return;
    }
    let fragment = value.strip_prefix('#').unwrap_or(value);
    inner.url.set_fragment(Some(fragment));
  }

  /// Return the (raw) query string without the leading `?`, if present.
  pub fn query(&self) -> Option<String> {
    self.inner.borrow().url.query().map(str::to_string)
  }

  /// Set the (raw) query string without the leading `?`.
  pub fn set_query(&self, query: Option<&str>) {
    self.inner.borrow_mut().url.set_query(query);
  }

  /// Return a live `URLSearchParams` view over this URL's query string.
  pub fn search_params(&self) -> UrlSearchParams {
    UrlSearchParams::associated(self.inner.clone())
  }

  /// Replace this URL's query with the serialization of `params`.
  pub fn set_search_params(&self, params: &UrlSearchParams) {
    let serialized = params.serialize();
    if serialized.is_empty() {
      self.inner.borrow_mut().url.set_query(None);
    } else {
      self
        .inner
        .borrow_mut()
        .url
        .set_query(Some(serialized.as_str()));
    }
  }

  /// Equivalent to the WHATWG `URL.toJSON()` method.
  pub fn to_json(&self) -> String {
    self.href()
  }
}

impl std::fmt::Display for Url {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.inner.borrow().url.as_str())
  }
}

#[derive(Clone, Debug)]
enum UrlSearchParamsInner {
  /// A standalone `URLSearchParams` list with its own storage.
  Standalone {
    pairs: Rc<RefCell<Vec<(String, String)>>>,
  },
  /// A `URLSearchParams` view over an associated `Url`.
  Associated { url: Rc<RefCell<UrlInner>> },
}

/// A WHATWG-shaped URLSearchParams list that preserves duplicates and stable ordering.
#[derive(Clone, Debug)]
pub struct UrlSearchParams {
  inner: UrlSearchParamsInner,
}

impl Default for UrlSearchParams {
  fn default() -> Self {
    Self {
      inner: UrlSearchParamsInner::Standalone {
        pairs: Rc::new(RefCell::new(Vec::new())),
      },
    }
  }
}

impl UrlSearchParams {
  /// Parse a raw query string such as `"a=b&c=d"` or a leading-`?` variant such as `"?a=b"`.
  pub fn parse(input: &str) -> Self {
    let pairs = parse_urlencoded_pairs(input);
    Self {
      inner: UrlSearchParamsInner::Standalone {
        pairs: Rc::new(RefCell::new(pairs)),
      },
    }
  }

  fn associated(url: Rc<RefCell<UrlInner>>) -> Self {
    Self {
      inner: UrlSearchParamsInner::Associated { url },
    }
  }

  pub fn append(&self, name: impl Into<String>, value: impl Into<String>) {
    let name = name.into();
    let value = value.into();
    self.mutate_pairs(|pairs| pairs.push((name, value)));
  }

  pub fn delete(&self, name: &str) {
    self.mutate_pairs(|pairs| pairs.retain(|(n, _)| n != name));
  }

  pub fn get(&self, name: &str) -> Option<String> {
    self
      .pairs()
      .into_iter()
      .find_map(|(n, v)| if n == name { Some(v) } else { None })
  }

  pub fn get_all(&self, name: &str) -> Vec<String> {
    self
      .pairs()
      .into_iter()
      .filter_map(|(n, v)| if n == name { Some(v) } else { None })
      .collect()
  }

  pub fn has(&self, name: &str) -> bool {
    self.pairs().into_iter().any(|(n, _)| n == name)
  }

  /// Set the first matching pair's value and remove any remaining pairs with the same name.
  ///
  /// If no existing pair matches `name`, append a new pair to the end of the list.
  pub fn set(&self, name: &str, value: impl Into<String>) {
    let value = value.into();
    self.mutate_pairs(|pairs| {
      let mut out = Vec::with_capacity(pairs.len().saturating_add(1));
      let mut seen = false;

      for (n, v) in pairs.drain(..) {
        if n == name {
          if !seen {
            seen = true;
            out.push((n, value.clone()));
          }
        } else {
          out.push((n, v));
        }
      }

      if !seen {
        out.push((name.to_string(), value));
      }

      *pairs = out;
    });
  }

  fn pairs(&self) -> Vec<(String, String)> {
    match &self.inner {
      UrlSearchParamsInner::Standalone { pairs } => pairs.borrow().clone(),
      UrlSearchParamsInner::Associated { url } => {
        let inner = url.borrow();
        parse_urlencoded_pairs(inner.url.query().unwrap_or(""))
      }
    }
  }

  fn mutate_pairs<F>(&self, f: F)
  where
    F: FnOnce(&mut Vec<(String, String)>),
  {
    match &self.inner {
      UrlSearchParamsInner::Standalone { pairs } => {
        let mut pairs = pairs.borrow_mut();
        f(&mut pairs);
      }
      UrlSearchParamsInner::Associated { url } => {
        let mut inner = url.borrow_mut();
        let mut pairs = parse_urlencoded_pairs(inner.url.query().unwrap_or(""));
        f(&mut pairs);

        let serialized = serialize_urlencoded_pairs(&pairs);
        if serialized.is_empty() {
          inner.url.set_query(None);
        } else {
          inner.url.set_query(Some(serialized.as_str()));
        }
      }
    }
  }

  fn serialize(&self) -> String {
    serialize_urlencoded_pairs(&self.pairs())
  }
}

impl std::fmt::Display for UrlSearchParams {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.serialize().as_str())
  }
}

fn parse_urlencoded_pairs(input: &str) -> Vec<(String, String)> {
  let input = input.strip_prefix('?').unwrap_or(input);
  ::url::form_urlencoded::parse(input.as_bytes())
    .map(|(name, value)| (name.into_owned(), value.into_owned()))
    .collect()
}

fn serialize_urlencoded_pairs(pairs: &[(String, String)]) -> String {
  let mut serializer = ::url::form_urlencoded::Serializer::new(String::new());
  for (name, value) in pairs {
    serializer.append_pair(name, value);
  }
  serializer.finish()
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
    let params = UrlSearchParams::parse("a=1&b=2&a=3");
    params.set("a", "9");
    assert_eq!(params.to_string(), "a=9&b=2");
  }

  #[test]
  fn urlsearchparams_serialization_percent_encodes() {
    let params = UrlSearchParams::default();
    params.append("a", "b ~");
    // SPACE becomes '+' and '~' is percent-encoded per the x-www-form-urlencoded encode set.
    assert_eq!(params.to_string(), "a=b+%7E");
  }

  #[test]
  fn urlsearchparams_serialization_handles_non_ascii() {
    let params = UrlSearchParams::default();
    params.append("café", "☕");
    assert_eq!(params.to_string(), "caf%C3%A9=%E2%98%95");
  }

  #[test]
  fn urlsearchparams_is_live_and_updates_href_on_mutation() {
    let url = Url::parse("https://example.com/?a=b%20~", None).unwrap();
    let params = url.search_params();

    // Reading searchParams does not normalize URL.search.
    assert_eq!(url.search(), "?a=b%20~");
    assert_eq!(params.get("a"), Some("b ~".to_string()));
    assert_eq!(params.to_string(), "a=b+%7E");
    assert_eq!(url.search(), "?a=b%20~");

    // Mutating searchParams rewrites URL.search using urlencoded serialization.
    params.append("c", "d");
    assert_eq!(url.href(), "https://example.com/?a=b+%7E&c=d");
    assert_eq!(url.search(), "?a=b+%7E&c=d");
  }

  #[test]
  fn url_search_setter_updates_associated_searchparams() {
    let url = Url::parse("https://example.com/", None).unwrap();
    let params = url.search_params();

    url.set_search("?q=a+b");
    assert_eq!(url.search(), "?q=a+b");
    assert_eq!(params.get("q"), Some("a b".to_string()));
    assert_eq!(params.to_string(), "q=a+b");

    url.set_search("");
    assert_eq!(url.search(), "");
    assert!(!params.has("q"));
  }

  #[test]
  fn urlsearchparams_encoding_spaces_and_plus() {
    let params = UrlSearchParams::parse("a=1+2&b=3%2B4");
    assert_eq!(params.get("a"), Some("1 2".to_string()));
    assert_eq!(params.get("b"), Some("3+4".to_string()));

    params.set("a", "x y");
    params.append("c", "1+2");
    assert_eq!(params.to_string(), "a=x+y&b=3%2B4&c=1%2B2");
  }

  #[test]
  fn urlsearchparams_live_set_replaces_all_entries_for_name() {
    let url = Url::parse("https://example.com/?a=1&b=2&a=3", None).unwrap();
    let params = url.search_params();
    params.set("a", "9");
    assert_eq!(url.href(), "https://example.com/?a=9&b=2");
    assert_eq!(params.to_string(), "a=9&b=2");
  }

  #[test]
  fn url_hash_getter_and_setter() {
    let url = Url::parse("https://example.com/#a", None).unwrap();
    assert_eq!(url.hash(), "#a");
    url.set_hash("#b");
    assert_eq!(url.hash(), "#b");
    assert_eq!(url.href(), "https://example.com/#b");

    url.set_hash("");
    assert_eq!(url.hash(), "");
    assert_eq!(url.href(), "https://example.com/");
  }
}
