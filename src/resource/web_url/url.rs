use crate::resource::web_url::error::{WebUrlError, WebUrlLimitKind};
use crate::resource::web_url::limits::WebUrlLimits;
use crate::resource::web_url::search_params::WebUrlSearchParams;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUrl {
  url: ::url::Url,
}

impl WebUrl {
  pub fn parse(input: &str, base: Option<&str>, limits: &WebUrlLimits) -> Result<Self, WebUrlError> {
    if input.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: input.len(),
      });
    }
    if let Some(base) = base {
      if base.len() > limits.max_input_bytes {
        return Err(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: limits.max_input_bytes,
          attempted: base.len(),
        });
      }
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

    // Enforce the max serialized length after normalization.
    if parsed.as_str().len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: parsed.as_str().len(),
      });
    }

    // Validate the query string under the URLSearchParams limits so `url.searchParams` can always
    // parse the associated query without unbounded work.
    if let Some(query) = parsed.query() {
      let _ = WebUrlSearchParams::parse(query, limits)?;
    }

    Ok(Self { url: parsed })
  }

  pub fn can_parse(input: &str, base: Option<&str>, limits: &WebUrlLimits) -> bool {
    Self::parse(input, base, limits).is_ok()
  }

  pub fn href(&self, limits: &WebUrlLimits) -> Result<String, WebUrlError> {
    let href = self.url.as_str();
    if href.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: href.len(),
      });
    }
    try_clone_str(href)
  }

  pub fn set_href(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }

    // Spec-wise, the href setter parses against the existing URL as a base.
    let base_url = self.url.clone();
    let parsed = ::url::Url::options()
      .base_url(Some(&base_url))
      .parse(value)
      .map_err(|_| WebUrlError::ParseError)?;

    Self::validate_url(&parsed, limits)?;
    self.url = parsed;
    Ok(())
  }

  pub fn origin(&self) -> String {
    self.url.origin().ascii_serialization()
  }

  pub fn protocol(&self) -> Result<String, WebUrlError> {
    let scheme = self.url.scheme();
    let mut out = String::new();
    out.try_reserve_exact(scheme.len() + 1)?;
    out.push_str(scheme);
    out.push(':');
    Ok(out)
  }

  pub fn set_protocol(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }
    let scheme = value.strip_suffix(':').unwrap_or(value);
    let mut cloned = self.url.clone();
    cloned
      .set_scheme(scheme)
      .map_err(|_| WebUrlError::ParseError)?;
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn username(&self) -> Result<String, WebUrlError> {
    try_clone_str(self.url.username())
  }

  pub fn set_username(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }
    let mut cloned = self.url.clone();
    cloned
      .set_username(value)
      .map_err(|_| WebUrlError::ParseError)?;
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn password(&self) -> Result<String, WebUrlError> {
    match self.url.password() {
      Some(pw) => try_clone_str(pw),
      None => Ok(String::new()),
    }
  }

  pub fn set_password(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }
    let mut cloned = self.url.clone();
    // Spec: empty string is still a password (not "no password").
    cloned.set_password(Some(value)).map_err(|_| WebUrlError::ParseError)?;
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn host(&self) -> Result<String, WebUrlError> {
    let Some(host) = self.url.host_str() else {
      return Ok(String::new());
    };
    let port = self.url.port();
    let mut out = String::new();
    let port_len = port.map(|p| 1 + p.to_string().len()).unwrap_or(0);
    out.try_reserve_exact(host.len() + port_len)?;
    out.push_str(host);
    if let Some(port) = port {
      out.push(':');
      out.push_str(&port.to_string());
    }
    Ok(out)
  }

  pub fn set_host(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }

    let (host, port) = split_host_and_port(value);
    let mut cloned = self.url.clone();
    if host.is_empty() {
      cloned.set_host(None).map_err(|_| WebUrlError::ParseError)?;
    } else {
      cloned
        .set_host(Some(host))
        .map_err(|_| WebUrlError::ParseError)?;
    }
    cloned.set_port(port).map_err(|_| WebUrlError::ParseError)?;
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn hostname(&self) -> Result<String, WebUrlError> {
    match self.url.host_str() {
      Some(host) => try_clone_str(host),
      None => Ok(String::new()),
    }
  }

  pub fn set_hostname(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }

    let mut cloned = self.url.clone();
    if value.is_empty() {
      cloned.set_host(None).map_err(|_| WebUrlError::ParseError)?;
    } else {
      cloned
        .set_host(Some(value))
        .map_err(|_| WebUrlError::ParseError)?;
    }
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn port(&self) -> Result<String, WebUrlError> {
    match self.url.port() {
      Some(port) => try_clone_str(&port.to_string()),
      None => Ok(String::new()),
    }
  }

  pub fn set_port(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }

    let port = if value.is_empty() {
      None
    } else {
      Some(value.parse::<u16>().map_err(|_| WebUrlError::ParseError)?)
    };

    let mut cloned = self.url.clone();
    cloned.set_port(port).map_err(|_| WebUrlError::ParseError)?;
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn pathname(&self) -> Result<String, WebUrlError> {
    try_clone_str(self.url.path())
  }

  pub fn set_pathname(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }

    let mut cloned = self.url.clone();
    cloned.set_path(value);
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn search(&self) -> Result<String, WebUrlError> {
    let Some(query) = self.url.query() else {
      return Ok(String::new());
    };
    let mut out = String::new();
    out.try_reserve_exact(query.len() + 1)?;
    out.push('?');
    out.push_str(query);
    Ok(out)
  }

  pub fn set_search(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }

    let mut cloned = self.url.clone();
    if value.is_empty() {
      cloned.set_query(None);
    } else {
      let query = value.strip_prefix('?').unwrap_or(value);
      if query.len() > limits.max_input_bytes {
        return Err(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: limits.max_input_bytes,
          attempted: query.len(),
        });
      }
      let _ = WebUrlSearchParams::parse(query, limits)?;
      cloned.set_query(Some(query));
    }

    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn hash(&self) -> Result<String, WebUrlError> {
    let Some(fragment) = self.url.fragment() else {
      return Ok(String::new());
    };
    let mut out = String::new();
    out.try_reserve_exact(fragment.len() + 1)?;
    out.push('#');
    out.push_str(fragment);
    Ok(out)
  }

  pub fn set_hash(&mut self, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if value.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: value.len(),
      });
    }

    let mut cloned = self.url.clone();
    if value.is_empty() {
      cloned.set_fragment(None);
    } else {
      let fragment = value.strip_prefix('#').unwrap_or(value);
      cloned.set_fragment(Some(fragment));
    }

    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  pub fn query(&self) -> Option<&str> {
    self.url.query()
  }

  pub fn search_params(&self, limits: &WebUrlLimits) -> Result<WebUrlSearchParams, WebUrlError> {
    match self.url.query() {
      Some(query) => WebUrlSearchParams::parse(query, limits),
      None => Ok(WebUrlSearchParams::new()),
    }
  }

  pub fn set_search_params(
    &mut self,
    params: &WebUrlSearchParams,
    limits: &WebUrlLimits,
  ) -> Result<(), WebUrlError> {
    let serialized = params.serialize(limits)?;
    let mut cloned = self.url.clone();
    if serialized.is_empty() {
      cloned.set_query(None);
    } else {
      cloned.set_query(Some(serialized.as_str()));
    }
    Self::validate_url(&cloned, limits)?;
    self.url = cloned;
    Ok(())
  }

  fn validate_url(url: &::url::Url, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    if url.as_str().len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: url.as_str().len(),
      });
    }
    if let Some(query) = url.query() {
      let _ = WebUrlSearchParams::parse(query, limits)?;
    }
    Ok(())
  }
}

fn try_clone_str(value: &str) -> Result<String, WebUrlError> {
  let mut out = String::new();
  out.try_reserve_exact(value.len())?;
  out.push_str(value);
  Ok(out)
}

fn split_host_and_port(input: &str) -> (&str, Option<u16>) {
  if let Some((host, port)) = input.rsplit_once(':') {
    if port.is_empty() {
      return (host, None);
    }
    if let Ok(port) = port.parse::<u16>() {
      return (host, Some(port));
    }
  }
  (input, None)
}

