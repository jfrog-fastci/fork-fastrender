//! Spec-shaped core URL + URLSearchParams primitives.
//!
//! This module is intended to back forthcoming WebIDL JS bindings for WHATWG `URL` and
//! `URLSearchParams`. It is not an embedding layer; it provides deterministic parsing and
//! serialization built on the `url` crate, plus a small `URLSearchParams` list implementation that
//! preserves duplicates and stable ordering.
//!
//! # Live `searchParams`
//!
//! `Url::search_params()` returns the cached [`UrlSearchParams`] object (equivalent to WHATWG
//! `URL.searchParams` with `[SameObject]`) that stays in sync with the URL's query string:
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
  #[error("failed to set {field} to {value:?}: {source}")]
  SetterFailure {
    field: &'static str,
    value: String,
    #[source]
    source: ::url::ParseError,
  },
  #[error("failed to set {field} to {value:?}")]
  SetterFailureOpaque { field: &'static str, value: String },
}

#[derive(Debug)]
struct UrlInner {
  url: ::url::Url,
}

/// A WHATWG-shaped URL wrapper used by JS bindings.
#[derive(Debug, Clone)]
pub struct Url {
  inner: Rc<RefCell<UrlInner>>,
  /// Cached `URL.searchParams` object.
  ///
  /// In the WHATWG URL IDL this attribute is marked `[SameObject]`, meaning repeated reads return
  /// the same object identity.
  search_params: Rc<UrlSearchParams>,
}

impl Url {
  /// Parse an input string into a URL.
  ///
  /// When `base` is provided it is first parsed as a URL; if it is invalid this returns
  /// `UrlError::InvalidBase`. Otherwise `input` is parsed using the WHATWG URL rules with the parsed
  /// base.
  pub fn parse(input: &str, base: Option<&str>) -> Result<Self, UrlError> {
    let url = match base {
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

    let inner = Rc::new(RefCell::new(UrlInner { url }));
    let search_params = Rc::new(UrlSearchParams::associated(inner.clone()));

    Ok(Self { inner, search_params })
  }

  /// Equivalent to the WHATWG `URL.parse(url, base)` static method.
  ///
  /// Returns `None` on any parse failure (including an invalid base URL).
  pub fn parse_static(input: &str, base: Option<&str>) -> Option<Self> {
    Self::parse(input, base).ok()
  }

  /// Equivalent to the WHATWG `URL.canParse(url, base)` static method.
  pub fn can_parse(input: &str, base: Option<&str>) -> bool {
    Self::parse_static(input, base).is_some()
  }

  /// Equivalent to the WHATWG `URL.href` getter.
  pub fn href(&self) -> String {
    self.inner.borrow().url.as_str().to_string()
  }

  /// Equivalent to the WHATWG `URL.href` setter.
  pub fn set_href(&self, value: &str) -> Result<(), UrlError> {
    let parsed = ::url::Url::parse(value).map_err(|source| UrlError::SetterFailure {
      field: "href",
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

  /// Equivalent to the WHATWG `URL.protocol` setter.
  ///
  /// Accepts both `"http"` and `"http:"` forms.
  pub fn set_protocol(&self, value: &str) -> Result<(), UrlError> {
    let scheme = value.strip_suffix(':').unwrap_or(value);
    self
      .inner
      .borrow_mut()
      .url
      .set_scheme(scheme)
      .map_err(|()| UrlError::SetterFailureOpaque {
        field: "protocol",
        value: value.to_string(),
      })
  }

  /// Equivalent to the WHATWG `URL.username` getter.
  pub fn username(&self) -> String {
    self.inner.borrow().url.username().to_string()
  }

  /// Equivalent to the WHATWG `URL.username` setter.
  pub fn set_username(&self, value: &str) -> Result<(), UrlError> {
    let mut inner = self.inner.borrow_mut();
    if cannot_have_username_password_port(&inner.url) {
      return Ok(());
    }

    inner
      .url
      .set_username(value)
      .map_err(|()| UrlError::SetterFailureOpaque {
        field: "username",
        value: value.to_string(),
      })
  }

  /// Equivalent to the WHATWG `URL.password` getter.
  pub fn password(&self) -> String {
    self
      .inner
      .borrow()
      .url
      .password()
      .unwrap_or_default()
      .to_string()
  }

  /// Equivalent to the WHATWG `URL.password` setter.
  pub fn set_password(&self, value: &str) -> Result<(), UrlError> {
    let mut inner = self.inner.borrow_mut();
    if cannot_have_username_password_port(&inner.url) {
      return Ok(());
    }

    inner
      .url
      .set_password(if value.is_empty() { None } else { Some(value) })
      .map_err(|()| UrlError::SetterFailureOpaque {
        field: "password",
        value: value.to_string(),
      })
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

  /// Equivalent to the WHATWG `URL.host` setter.
  ///
  /// Accepts both `"example.com"` and `"example.com:8080"` forms. If the port component is omitted,
  /// the URL's port is left unchanged (per the WHATWG URL Standard note).
  pub fn set_host(&self, value: &str) -> Result<(), UrlError> {
    let mut inner = self.inner.borrow_mut();
    if inner.url.cannot_be_a_base() {
      return Ok(());
    }

    let (host, port) = split_host_and_port(value);
    // Build on a copy so failures don't partially update the URL.
    let mut new_url = inner.url.clone();
    set_host_impl(&mut new_url, host, "host", value)?;

    if let Some(port) = port {
      if port.is_empty() {
        new_url
          .set_port(None)
          .map_err(|()| UrlError::SetterFailureOpaque {
            field: "host",
            value: value.to_string(),
          })?;
      } else {
        let port_num: u16 = port.parse().map_err(|_| UrlError::SetterFailureOpaque {
          field: "host",
          value: value.to_string(),
        })?;
        new_url
          .set_port(Some(port_num))
          .map_err(|()| UrlError::SetterFailureOpaque {
            field: "host",
            value: value.to_string(),
          })?;
      }
    }

    inner.url = new_url;
    Ok(())
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

  /// Equivalent to the WHATWG `URL.hostname` setter.
  pub fn set_hostname(&self, value: &str) -> Result<(), UrlError> {
    let mut inner = self.inner.borrow_mut();
    if inner.url.cannot_be_a_base() {
      return Ok(());
    }
    set_host_impl(&mut inner.url, value, "hostname", value)
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

  /// Equivalent to the WHATWG `URL.port` setter.
  ///
  /// - `""` clears the port.
  /// - Otherwise, the value must be numeric.
  pub fn set_port(&self, value: &str) -> Result<(), UrlError> {
    let mut inner = self.inner.borrow_mut();
    if cannot_have_username_password_port(&inner.url) {
      return Ok(());
    }

    if value.is_empty() {
      inner
        .url
        .set_port(None)
        .map_err(|()| UrlError::SetterFailureOpaque {
          field: "port",
          value: value.to_string(),
        })?;
      return Ok(());
    }

    let port_num: u16 = value.parse().map_err(|_| UrlError::SetterFailureOpaque {
      field: "port",
      value: value.to_string(),
    })?;
    inner
      .url
      .set_port(Some(port_num))
      .map_err(|()| UrlError::SetterFailureOpaque {
        field: "port",
        value: value.to_string(),
      })?;
    Ok(())
  }

  /// Equivalent to the WHATWG `URL.pathname` getter.
  pub fn pathname(&self) -> String {
    self.inner.borrow().url.path().to_string()
  }

  /// Equivalent to the WHATWG `URL.pathname` setter.
  pub fn set_pathname(&self, value: &str) -> Result<(), UrlError> {
    let mut inner = self.inner.borrow_mut();
    if inner.url.cannot_be_a_base() {
      return Ok(());
    }

    if value.starts_with('/') {
      inner.url.set_path(value);
      return Ok(());
    }

    // WHATWG `pathname` setter uses the path start state, which effectively makes the path absolute
    // for a URL with an authority.
    let mut path = String::with_capacity(value.len() + 1);
    path.push('/');
    path.push_str(value);
    inner.url.set_path(&path);
    Ok(())
  }

  /// Equivalent to the WHATWG `URL.search` getter.
  pub fn search(&self) -> String {
    match self.inner.borrow().url.query() {
      None => String::new(),
      Some(q) if q.is_empty() => String::new(),
      Some(q) => format!("?{q}"),
    }
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
    match self.inner.borrow().url.fragment() {
      None => String::new(),
      Some(f) if f.is_empty() => String::new(),
      Some(f) => format!("#{f}"),
    }
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
  pub fn search_params(&self) -> Rc<UrlSearchParams> {
    self.search_params.clone()
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

  /// Equivalent to the WHATWG `URLSearchParams.delete(name, value?)`.
  pub fn delete(&self, name: &str, value: Option<&str>) {
    match value {
      Some(value) => self.mutate_pairs(|pairs| {
        pairs.retain(|(n, v)| !(n == name && v == value));
      }),
      None => self.mutate_pairs(|pairs| pairs.retain(|(n, _)| n != name)),
    }
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

  /// Equivalent to the WHATWG `URLSearchParams.has(name, value?)`.
  pub fn has(&self, name: &str, value: Option<&str>) -> bool {
    match value {
      Some(value) => self
        .pairs()
        .into_iter()
        .any(|(n, v)| n == name && v == value),
      None => self.pairs().into_iter().any(|(n, _)| n == name),
    }
  }

  /// Equivalent to the WHATWG `URLSearchParams.size` getter.
  pub fn len(&self) -> usize {
    self.pairs().len()
  }

  /// Convenience alias for `len()` matching the WHATWG `size` name.
  pub fn size(&self) -> usize {
    self.len()
  }

  pub fn is_empty(&self) -> bool {
    self.len() == 0
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

  /// A snapshot of the underlying list, in list order.
  pub fn pairs(&self) -> Vec<(String, String)> {
    match &self.inner {
      UrlSearchParamsInner::Standalone { pairs } => pairs.borrow().clone(),
      UrlSearchParamsInner::Associated { url } => {
        let inner = url.borrow();
        parse_urlencoded_pairs(inner.url.query().unwrap_or(""))
      }
    }
  }

  /// Equivalent to the WHATWG `URLSearchParams.sort()` method.
  pub fn sort(&self) {
    self.mutate_pairs(|pairs| {
      pairs.sort_by(|(a, _), (b, _)| compare_utf16_code_units(a, b));
    });
  }

  /// A sorted snapshot of the underlying list.
  ///
  /// This does not mutate the underlying storage; use [`UrlSearchParams::sort`] to mutate.
  pub fn entries_sorted(&self) -> Vec<(String, String)> {
    let mut pairs = self.pairs();
    pairs.sort_by(|(a, _), (b, _)| compare_utf16_code_units(a, b));
    pairs
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

fn compare_utf16_code_units(a: &str, b: &str) -> std::cmp::Ordering {
  // WHATWG defines URLSearchParams.sort() ordering in terms of Web IDL "code unit less than", which
  // corresponds to lexicographic ordering on UTF-16 code units (i.e. JS string `<` comparison).
  let mut a_units = a.encode_utf16();
  let mut b_units = b.encode_utf16();
  loop {
    match (a_units.next(), b_units.next()) {
      (Some(a), Some(b)) => match a.cmp(&b) {
        std::cmp::Ordering::Equal => {}
        ord => return ord,
      },
      (None, Some(_)) => return std::cmp::Ordering::Less,
      (Some(_), None) => return std::cmp::Ordering::Greater,
      (None, None) => return std::cmp::Ordering::Equal,
    }
  }
}

fn cannot_have_username_password_port(url: &::url::Url) -> bool {
  url.scheme() == "file" || url.host_str().map_or(true, str::is_empty)
}

fn split_host_and_port(value: &str) -> (&str, Option<&str>) {
  if let Some(rest) = value.strip_prefix('[') {
    if let Some(end) = rest.find(']') {
      let end = end + 1; // include leading '['
      let host = &value[..=end];
      let after = &value[end + 1..];
      if let Some(port) = after.strip_prefix(':') {
        return (host, Some(port));
      }
      return (value, None);
    }
  }

  match value.rsplit_once(':') {
    Some((host, port)) => (host, Some(port)),
    None => (value, None),
  }
}

fn set_host_impl(
  url: &mut ::url::Url,
  host: &str,
  field: &'static str,
  value_for_error: &str,
) -> Result<(), UrlError> {
  let host_arg = if host.is_empty() { Some("") } else { Some(host) };
  match url.set_host(host_arg) {
    Ok(()) => Ok(()),
    Err(source) if host.is_empty() => url.set_host(None).map_err(|source| UrlError::SetterFailure {
      field,
      value: value_for_error.to_string(),
      source,
    }),
    Err(source) => Err(UrlError::SetterFailure {
      field,
      value: value_for_error.to_string(),
      source,
    }),
  }
}

#[cfg(test)]
mod tests {
  use super::{Url, UrlError, UrlSearchParams};
  use std::rc::Rc;

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
    assert!(params.has("b", None));
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
    assert!(!params.has("q", None));
  }

  #[test]
  fn url_search_and_hash_setters_percent_encode_spaces() {
    let url = Url::parse("https://example.com/path#frag", None).unwrap();
    let params = url.search_params();

    url.set_search("?q=a b");
    // WHATWG URL search setter uses the query state parser, which percent-encodes spaces as %20 (not
    // `+`, which is specific to x-www-form-urlencoded serialization).
    assert_eq!(url.search(), "?q=a%20b");
    assert_eq!(url.href(), "https://example.com/path?q=a%20b#frag");
    assert_eq!(params.get("q"), Some("a b".to_string()));

    url.set_hash("#h a");
    assert_eq!(url.hash(), "#h%20a");
    assert_eq!(url.href(), "https://example.com/path?q=a%20b#h%20a");
  }

  #[test]
  fn url_searchparams_is_same_object() {
    let url = Url::parse("https://example.com/?a=b", None).unwrap();
    let a = url.search_params();
    let b = url.search_params();
    assert!(Rc::ptr_eq(&a, &b));
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

  #[test]
  fn url_search_and_hash_getters_hide_empty_components() {
    let url = Url::parse("https://example.com/?", None).unwrap();
    assert_eq!(url.href(), "https://example.com/?");
    // Per WHATWG, `search` is the empty string when the query is null *or* the empty string.
    assert_eq!(url.search(), "");

    let url = Url::parse("https://example.com/#", None).unwrap();
    assert_eq!(url.href(), "https://example.com/#");
    // Per WHATWG, `hash` is the empty string when the fragment is null *or* the empty string.
    assert_eq!(url.hash(), "");
  }

  #[test]
  fn url_static_parse_matches_can_parse_and_null_on_failure() {
    assert!(Url::parse_static("https://example.com/", None).is_some());
    assert!(Url::can_parse("https://example.com/", None));

    assert!(Url::parse_static("not a url", None).is_none());
    assert!(!Url::can_parse("not a url", None));

    let err = Url::parse("not a url", None).unwrap_err();
    assert!(matches!(err, UrlError::Parse { .. }));
  }

  #[test]
  fn url_attribute_setters_roundtrip_in_href() {
    let url = Url::parse("https://example.com/dir/file?x=y#z", None).unwrap();

    url.set_protocol("http").unwrap();
    assert_eq!(url.protocol(), "http:");

    url.set_host("example.org:8080").unwrap();
    assert_eq!(url.host(), "example.org:8080");

    url.set_port("9090").unwrap();
    assert_eq!(url.port(), "9090");

    url.set_pathname("a/b").unwrap();
    assert_eq!(url.pathname(), "/a/b");

    assert_eq!(url.href(), "http://example.org:9090/a/b?x=y#z");
  }

  #[test]
  fn url_username_and_password_setters_update_href() {
    let url = Url::parse("https://example.com/", None).unwrap();
    url.set_username("alice").unwrap();
    url.set_password("secret").unwrap();
    assert_eq!(url.username(), "alice");
    assert_eq!(url.password(), "secret");
    assert_eq!(url.href(), "https://alice:secret@example.com/");

    url.set_password("").unwrap();
    assert_eq!(url.password(), "");
    assert_eq!(url.href(), "https://alice@example.com/");
  }

  #[test]
  fn urlsearchparams_delete_and_has_optional_value() {
    let params = UrlSearchParams::parse("a=1&a=2&b=3");
    assert!(params.has("a", None));
    assert!(params.has("a", Some("1")));
    assert!(!params.has("a", Some("9")));

    params.delete("a", Some("1"));
    assert_eq!(params.to_string(), "a=2&b=3");

    params.delete("a", None);
    assert_eq!(params.to_string(), "b=3");
  }

  #[test]
  fn urlsearchparams_sort_is_stable_and_live() {
    let url = Url::parse("https://example.com/?b=2&a=1&a=0", None).unwrap();
    let params = url.search_params();
    params.sort();

    assert_eq!(params.to_string(), "a=1&a=0&b=2");
    assert_eq!(url.search(), "?a=1&a=0&b=2");
    assert_eq!(url.href(), "https://example.com/?a=1&a=0&b=2");
  }

  #[test]
  fn urlsearchparams_sort_orders_by_utf16_code_units() {
    // JS compares strings by UTF-16 code units; that differs from Unicode scalar value order for
    // non-BMP characters. U+10000 encodes to the surrogate pair [0xD800, 0xDC00], which is less than
    // U+E000 (0xE000) when comparing code units.
    let params = UrlSearchParams::parse("\u{E000}=1&\u{10000}=2");
    params.sort();
    assert_eq!(params.to_string(), "%F0%90%80%80=2&%EE%80%80=1");
  }
}
