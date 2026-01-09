use crate::resource::web_url::error::{WebUrlError, WebUrlLimitKind};
use crate::resource::web_url::limits::WebUrlLimits;
use crate::resource::web_url::search_params::WebUrlSearchParams;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug)]
pub(crate) struct WebUrlInner {
  before_query: String,
  query: Option<String>,
  fragment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WebUrl {
  inner: Rc<RefCell<WebUrlInner>>,
}

impl PartialEq for WebUrl {
  fn eq(&self, other: &Self) -> bool {
    let a = self.inner.borrow();
    let b = other.inner.borrow();
    a.before_query == b.before_query && a.query == b.query && a.fragment == b.fragment
  }
}

impl Eq for WebUrl {}

impl WebUrl {
  pub fn parse(input: &str, limits: &WebUrlLimits) -> Result<Self, WebUrlError> {
    if input.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: input.len(),
      });
    }

    let (before_fragment, fragment) = match input.split_once('#') {
      Some((before, frag)) => (before, Some(frag)),
      None => (input, None),
    };

    let (before_query, query) = match before_fragment.split_once('?') {
      Some((before, query)) => (before, Some(query)),
      None => (before_fragment, None),
    };

    Ok(Self {
      inner: Rc::new(RefCell::new(WebUrlInner {
      before_query: try_clone_str(before_query)?,
      query: match query {
        Some(value) => Some(try_clone_str(value)?),
        None => None,
      },
      fragment: match fragment {
        Some(value) => Some(try_clone_str(value)?),
        None => None,
      },
    })),
    })
  }

  /// Return the raw query string (without a leading `?`), if present.
  pub fn query(&self) -> Result<Option<String>, WebUrlError> {
    let inner = self.inner.borrow();
    match &inner.query {
      Some(value) => Ok(Some(try_clone_str(value)?)),
      None => Ok(None),
    }
  }

  pub fn href(&self, limits: &WebUrlLimits) -> Result<String, WebUrlError> {
    let inner = self.inner.borrow();
    let mut total_len = inner.before_query.len();
    if let Some(query) = &inner.query {
      total_len = total_len
        .checked_add(1)
        .and_then(|len| len.checked_add(query.len()))
        .ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: limits.max_input_bytes,
          attempted: usize::MAX,
        })?;
    }
    if let Some(fragment) = &inner.fragment {
      total_len = total_len
        .checked_add(1)
        .and_then(|len| len.checked_add(fragment.len()))
        .ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: limits.max_input_bytes,
          attempted: usize::MAX,
        })?;
    }

    if total_len > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: total_len,
      });
    }

    let mut out = String::new();
    out.try_reserve_exact(total_len)?;
    out.push_str(&inner.before_query);
    if let Some(query) = &inner.query {
      out.push('?');
      out.push_str(query);
    }
    if let Some(fragment) = &inner.fragment {
      out.push('#');
      out.push_str(fragment);
    }
    Ok(out)
  }

  /// Return a live `URLSearchParams` view over this URL's query.
  pub fn search_params(&self) -> WebUrlSearchParams {
    WebUrlSearchParams::associated(self.clone())
  }

  /// Parse `input` as a query string and replace this URL's query.
  pub fn set_search(&self, input: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    // Parse/serialize first so we can update state atomically.
    let params = WebUrlSearchParams::parse(input, limits)?;
    let serialized = params.serialize(limits)?;
    let new_query = if serialized.is_empty() {
      None
    } else {
      Some(serialized)
    };

    self.set_query_internal(new_query, limits)
  }

  pub fn set_search_params(
    &self,
    params: &WebUrlSearchParams,
    limits: &WebUrlLimits,
  ) -> Result<(), WebUrlError> {
    let serialized = params.serialize(limits)?;
    let new_query = if serialized.is_empty() {
      None
    } else {
      Some(serialized)
    };
    self.set_query_internal(new_query, limits)
  }

  fn set_query_internal(
    &self,
    new_query: Option<String>,
    limits: &WebUrlLimits,
  ) -> Result<(), WebUrlError> {
    let mut inner = self.inner.borrow_mut();
    let mut total_len = inner.before_query.len();
    if let Some(query) = &new_query {
      total_len = total_len
        .checked_add(1)
        .and_then(|len| len.checked_add(query.len()))
        .ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: limits.max_input_bytes,
          attempted: usize::MAX,
        })?;
    }
    if let Some(fragment) = &inner.fragment {
      total_len = total_len
        .checked_add(1)
        .and_then(|len| len.checked_add(fragment.len()))
        .ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: limits.max_input_bytes,
          attempted: usize::MAX,
        })?;
    }
    if total_len > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: total_len,
      });
    }

    inner.query = new_query;
    Ok(())
  }

  pub(crate) fn search_params_snapshot(
    &self,
    limits: &WebUrlLimits,
  ) -> Result<WebUrlSearchParams, WebUrlError> {
    let inner = self.inner.borrow();
    match &inner.query {
      Some(query) => WebUrlSearchParams::parse(query, limits),
      None => Ok(WebUrlSearchParams::new()),
    }
  }
}

fn try_clone_str(value: &str) -> Result<String, WebUrlError> {
  let mut out = String::new();
  out.try_reserve_exact(value.len())?;
  out.push_str(value);
  Ok(out)
}
