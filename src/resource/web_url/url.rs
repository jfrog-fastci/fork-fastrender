use std::cell::RefCell;
use std::rc::Rc;

use crate::resource::web_url::error::{WebUrlError, WebUrlLimitKind, WebUrlSetter};
use crate::resource::web_url::limits::WebUrlLimits;
use crate::resource::web_url::search_params::WebUrlSearchParams;

#[derive(Debug)]
pub(crate) struct WebUrlInner {
  pub(crate) url: ::url::Url,
}

/// A bounded, WHATWG-shaped URL wrapper intended for hostile input (e.g. JS bindings).
#[derive(Debug, Clone)]
pub struct WebUrl {
  pub(crate) inner: Rc<RefCell<WebUrlInner>>,
  limits: WebUrlLimits,
}

impl WebUrl {
  /// Parse an input string into a URL, optionally resolving against `base`.
  pub fn parse(
    input: &str,
    base: Option<&str>,
    limits: &WebUrlLimits,
  ) -> Result<Self, WebUrlError> {
    enforce_input_len(input, limits)?;

    let parsed = match base {
      Some(base) => {
        enforce_input_len(base, limits)?;

        let base_url = match ::url::Url::parse(base) {
          Ok(url) => url,
          Err(source) => {
            return Err(WebUrlError::InvalidBase {
              base: try_clone_str(base)?,
              source,
            })
          }
        };

        match ::url::Url::options().base_url(Some(&base_url)).parse(input) {
          Ok(url) => url,
          Err(source) => {
            return Err(WebUrlError::Parse {
              input: try_clone_str(input)?,
              base: Some(try_clone_str(base)?),
              source,
            })
          }
        }
      }
      None => match ::url::Url::parse(input) {
        Ok(url) => url,
        Err(source) => {
          return Err(WebUrlError::Parse {
            input: try_clone_str(input)?,
            base: None,
            source,
          })
        }
      },
    };

    enforce_href_len(parsed.as_str(), limits)?;

    Ok(Self {
      inner: Rc::new(RefCell::new(WebUrlInner { url: parsed })),
      limits: limits.clone(),
    })
  }

  /// Parse an input string into a URL, optionally resolving against `base`, but without retaining
  /// diagnostic copies of `input`/`base` on failure.
  ///
  /// This is useful for JS bindings that surface only a generic `"Invalid URL"` error message: it
  /// avoids cloning potentially-large `input`/`base` strings into [`WebUrlError`] variants.
  pub fn parse_without_diagnostics(
    input: &str,
    base: Option<&str>,
    limits: &WebUrlLimits,
  ) -> Result<Self, WebUrlError> {
    enforce_input_len(input, limits)?;
    if let Some(base) = base {
      enforce_input_len(base, limits)?;
    }

    let parsed = match base {
      Some(base) => {
        let base_url = ::url::Url::parse(base).map_err(|_| WebUrlError::ParseError)?;
        ::url::Url::options()
          .base_url(Some(&base_url))
          .parse(input)
          .map_err(|_| WebUrlError::ParseError)?
      }
      None => ::url::Url::parse(input).map_err(|_| WebUrlError::ParseError)?,
    };

    enforce_href_len(parsed.as_str(), limits)?;

    Ok(Self {
      inner: Rc::new(RefCell::new(WebUrlInner { url: parsed })),
      limits: limits.clone(),
    })
  }

  /// Fast-path predicate for whether `input` (optionally resolved against `base`) is a valid URL
  /// under the provided limits.
  ///
  /// This is useful for implementing WHATWG `URL.canParse()` without needing to allocate the
  /// detailed [`WebUrlError`] variants produced by [`WebUrl::parse`] (which clone input strings for
  /// diagnostics).
  pub fn can_parse(input: &str, base: Option<&str>, limits: &WebUrlLimits) -> bool {
    if input.len() > limits.max_input_bytes {
      return false;
    }
    if let Some(base) = base {
      if base.len() > limits.max_input_bytes {
        return false;
      }
    }

    let parsed = match base {
      Some(base) => {
        let Ok(base_url) = ::url::Url::parse(base) else {
          return false;
        };
        match ::url::Url::options().base_url(Some(&base_url)).parse(input) {
          Ok(url) => url,
          Err(_) => return false,
        }
      }
      None => match ::url::Url::parse(input) {
        Ok(url) => url,
        Err(_) => return false,
      },
    };

    parsed.as_str().len() <= limits.max_input_bytes
  }

  /// Equivalent to the WHATWG `URL.href` getter.
  pub fn href(&self) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    enforce_href_len(inner.url.as_str(), &self.limits)?;
    try_clone_str(inner.url.as_str())
  }

  /// Equivalent to the WHATWG `URL.href` setter.
  pub fn set_href(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    let parsed = match ::url::Url::parse(value) {
      Ok(url) => url,
      Err(source) => {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Href,
          value: try_clone_str(value)?,
          source: Some(source),
        })
      }
    };

    enforce_href_len(parsed.as_str(), &self.limits)?;
    self.inner.borrow_mut().url = parsed;
    Ok(())
  }

  /// Equivalent to the WHATWG `URL.toJSON()` method.
  pub fn to_json(&self) -> Result<String, WebUrlError> {
    self.href()
  }

  /// Equivalent to the WHATWG `URL.origin` getter.
  pub fn origin(&self) -> String {
    self.inner.borrow().url.origin().ascii_serialization()
  }

  /// Equivalent to the WHATWG `URL.protocol` getter.
  pub fn protocol(&self) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    let scheme = inner.url.scheme();
    let mut out = String::new();
    out.try_reserve_exact(scheme.len().saturating_add(1))?;
    out.push_str(scheme);
    out.push(':');
    Ok(out)
  }

  /// Equivalent to the WHATWG `URL.protocol` setter.
  pub fn set_protocol(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;
    let scheme = value.strip_suffix(':').unwrap_or(value);

    self.mutate_url(|url| {
      if url.set_scheme(scheme).is_err() {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Protocol,
          value: try_clone_str(value)?,
          source: None,
        });
      }
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.username` getter.
  pub fn username(&self) -> Result<String, WebUrlError> {
    try_clone_str(self.inner.borrow().url.username())
  }

  /// Equivalent to the WHATWG `URL.username` setter.
  pub fn set_username(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    self.mutate_url(|url| {
      if url.set_username(value).is_err() {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Username,
          value: try_clone_str(value)?,
          source: None,
        });
      }
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.password` getter.
  pub fn password(&self) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    let pw = inner.url.password().unwrap_or("");
    try_clone_str(pw)
  }

  /// Equivalent to the WHATWG `URL.password` setter.
  pub fn set_password(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    self.mutate_url(|url| {
      if url.set_password(Some(value)).is_err() {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Password,
          value: try_clone_str(value)?,
          source: None,
        });
      }
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.host` getter.
  pub fn host(&self) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    let Some(host) = inner.url.host_str() else {
      return Ok(String::new());
    };

    let port = inner.url.port();
    let mut out = String::new();
    // Reserve enough for `host`, plus `:`, plus a 5-digit port.
    out.try_reserve_exact(host.len().saturating_add(6))?;
    out.push_str(host);
    if let Some(port) = port {
      use std::fmt::Write as _;
      out.push(':');
      write!(&mut out, "{port}").expect("writing to String should not fail");
    }
    Ok(out)
  }

  /// Equivalent to the WHATWG `URL.host` setter.
  pub fn set_host(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    if value.is_empty() {
      return self.mutate_url(|url| {
        if let Err(source) = url.set_host(None) {
          return Err(WebUrlError::SetterFailure {
            setter: WebUrlSetter::Host,
            value: try_clone_str(value)?,
            source: Some(source),
          });
        }
        if url.set_port(None).is_err() {
          return Err(WebUrlError::SetterFailure {
            setter: WebUrlSetter::Host,
            value: try_clone_str(value)?,
            source: None,
          });
        }
        Ok(())
      });
    }

    // Parse `value` as the authority component by constructing a temporary URL. This delegates
    // host/IPv6/port parsing to `url` without requiring a custom parser here.
    let tmp = {
      let inner = self.inner.borrow();
      parse_authority_with_scheme(inner.url.scheme(), value, &self.limits, WebUrlSetter::Host)?
    };
    let host = tmp.host_str();
    let port = tmp.port();

    self.mutate_url(|url| {
      if let Err(source) = url.set_host(host) {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Host,
          value: try_clone_str(value)?,
          source: Some(source),
        });
      }
      if url.set_port(port).is_err() {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Host,
          value: try_clone_str(value)?,
          source: None,
        });
      }
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.hostname` getter.
  pub fn hostname(&self) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    let host = inner.url.host_str().unwrap_or("");
    try_clone_str(host)
  }

  /// Equivalent to the WHATWG `URL.hostname` setter.
  pub fn set_hostname(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    let host = if value.is_empty() { None } else { Some(value) };
    self.mutate_url(|url| {
      if let Err(source) = url.set_host(host) {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Hostname,
          value: try_clone_str(value)?,
          source: Some(source),
        });
      }
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.port` getter.
  pub fn port(&self) -> Result<String, WebUrlError> {
    let Some(port) = self.inner.borrow().url.port() else {
      return Ok(String::new());
    };
    let mut out = String::new();
    out.try_reserve_exact(5)?;
    use std::fmt::Write as _;
    write!(&mut out, "{port}").expect("writing to String should not fail");
    Ok(out)
  }

  /// Equivalent to the WHATWG `URL.port` setter.
  pub fn set_port(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    let port = if value.is_empty() {
      None
    } else {
      let parsed = match value.parse::<u16>() {
        Ok(parsed) => parsed,
        Err(_) => {
          return Err(WebUrlError::SetterFailure {
            setter: WebUrlSetter::Port,
            value: try_clone_str(value)?,
            source: None,
          })
        }
      };
      Some(parsed)
    };

    self.mutate_url(|url| {
      if url.set_port(port).is_err() {
        return Err(WebUrlError::SetterFailure {
          setter: WebUrlSetter::Port,
          value: try_clone_str(value)?,
          source: None,
        });
      }
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.pathname` getter.
  pub fn pathname(&self) -> Result<String, WebUrlError> {
    try_clone_str(self.inner.borrow().url.path())
  }

  /// Equivalent to the WHATWG `URL.pathname` setter.
  pub fn set_pathname(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    self.mutate_url(|url| {
      url.set_path(value);
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.search` getter.
  pub fn search(&self) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    let Some(query) = inner.url.query() else {
      return Ok(String::new());
    };
    let mut out = String::new();
    out.try_reserve_exact(query.len().saturating_add(1))?;
    out.push('?');
    out.push_str(query);
    Ok(out)
  }

  /// Equivalent to the WHATWG `URL.search` setter.
  ///
  /// - `""` clears the query.
  /// - Otherwise a leading `?` is stripped.
  pub fn set_search(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    self.mutate_url(|url| {
      if value.is_empty() {
        url.set_query(None);
        return Ok(());
      }
      let input = value.strip_prefix('?').unwrap_or(value);
      // The WHATWG `URL.search` setter runs the URL parser in "query state", where `#` starts the
      // fragment. `url::Url::set_query` percent-encodes `#` as `%23`, so we split on `#` ourselves
      // to preserve delimiter semantics.
      let (query, fragment) = match input.split_once('#') {
        Some((query, fragment)) => (query, Some(fragment)),
        None => (input, None),
      };
      url.set_query(Some(query));
      if let Some(fragment) = fragment {
        url.set_fragment(Some(fragment));
      }
      Ok(())
    })
  }

  /// Equivalent to the WHATWG `URL.hash` getter.
  pub fn hash(&self) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    let Some(fragment) = inner.url.fragment() else {
      return Ok(String::new());
    };
    let mut out = String::new();
    out.try_reserve_exact(fragment.len().saturating_add(1))?;
    out.push('#');
    out.push_str(fragment);
    Ok(out)
  }

  /// Equivalent to the WHATWG `URL.hash` setter.
  ///
  /// - `""` clears the fragment.
  /// - Otherwise a leading `#` is stripped.
  pub fn set_hash(&self, value: &str) -> Result<(), WebUrlError> {
    enforce_input_len(value, &self.limits)?;

    self.mutate_url(|url| {
      if value.is_empty() {
        url.set_fragment(None);
        return Ok(());
      }
      let fragment = value.strip_prefix('#').unwrap_or(value);
      url.set_fragment(Some(fragment));
      Ok(())
    })
  }

  /// Return a live `URLSearchParams` view over this URL's query string.
  pub fn search_params(&self) -> WebUrlSearchParams {
    WebUrlSearchParams::associated(self.inner.clone(), self.limits.clone())
  }

  fn mutate_url<F>(&self, f: F) -> Result<(), WebUrlError>
  where
    F: FnOnce(&mut ::url::Url) -> Result<(), WebUrlError>,
  {
    let mut inner = self.inner.borrow_mut();
    let before = inner.url.clone();

    if let Err(err) = f(&mut inner.url) {
      inner.url = before;
      return Err(err);
    }

    if let Err(err) = enforce_href_len(inner.url.as_str(), &self.limits) {
      inner.url = before;
      return Err(err);
    }

    Ok(())
  }
}

impl std::fmt::Display for WebUrl {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.inner.borrow().url.as_str())
  }
}

fn enforce_input_len(input: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
  if input.len() > limits.max_input_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::InputBytes,
      limit: limits.max_input_bytes,
      attempted: input.len(),
    });
  }
  Ok(())
}

fn enforce_href_len(href: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
  if href.len() > limits.max_input_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::InputBytes,
      limit: limits.max_input_bytes,
      attempted: href.len(),
    });
  }
  Ok(())
}

fn try_clone_str(value: &str) -> Result<String, WebUrlError> {
  let mut out = String::new();
  out.try_reserve_exact(value.len())?;
  out.push_str(value);
  Ok(out)
}

fn parse_authority_with_scheme(
  scheme: &str,
  authority: &str,
  limits: &WebUrlLimits,
  setter: WebUrlSetter,
) -> Result<::url::Url, WebUrlError> {
  // `{scheme}://{authority}`.
  let capacity = scheme
    .len()
    .checked_add(3)
    .and_then(|len| len.checked_add(authority.len()))
    .ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::InputBytes,
      limit: limits.max_input_bytes,
      attempted: usize::MAX,
    })?;
  if capacity > limits.max_input_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::InputBytes,
      limit: limits.max_input_bytes,
      attempted: capacity,
    });
  }

  let mut tmp = String::new();
  tmp.try_reserve_exact(capacity)?;
  tmp.push_str(scheme);
  tmp.push_str("://");
  tmp.push_str(authority);

  match ::url::Url::parse(&tmp) {
    Ok(url) => Ok(url),
    Err(source) => Err(WebUrlError::SetterFailure {
      setter,
      value: try_clone_str(authority)?,
      source: Some(source),
    }),
  }
}
