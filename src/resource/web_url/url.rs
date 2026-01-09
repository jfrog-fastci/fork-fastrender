use crate::resource::web_url::error::{WebUrlError, WebUrlLimitKind};
use crate::resource::web_url::limits::WebUrlLimits;
use crate::resource::web_url::search_params::WebUrlSearchParams;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUrl {
  before_query: String,
  query: Option<String>,
  fragment: Option<String>,
}

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
      before_query: try_clone_str(before_query)?,
      query: match query {
        Some(value) => Some(try_clone_str(value)?),
        None => None,
      },
      fragment: match fragment {
        Some(value) => Some(try_clone_str(value)?),
        None => None,
      },
    })
  }

  pub fn query(&self) -> Option<&str> {
    self.query.as_deref()
  }

  pub fn href(&self, limits: &WebUrlLimits) -> Result<String, WebUrlError> {
    let mut total_len = self.before_query.len();
    if let Some(query) = &self.query {
      total_len = total_len
        .checked_add(1)
        .and_then(|len| len.checked_add(query.len()))
        .ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: limits.max_input_bytes,
          attempted: usize::MAX,
        })?;
    }
    if let Some(fragment) = &self.fragment {
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
    out.push_str(&self.before_query);
    if let Some(query) = &self.query {
      out.push('?');
      out.push_str(query);
    }
    if let Some(fragment) = &self.fragment {
      out.push('#');
      out.push_str(fragment);
    }
    Ok(out)
  }

  pub fn search_params(&self, limits: &WebUrlLimits) -> Result<WebUrlSearchParams, WebUrlError> {
    match &self.query {
      Some(query) => WebUrlSearchParams::parse(query, limits),
      None => Ok(WebUrlSearchParams::new()),
    }
  }

  pub fn set_search(&mut self, input: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
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
    &mut self,
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
    &mut self,
    new_query: Option<String>,
    limits: &WebUrlLimits,
  ) -> Result<(), WebUrlError> {
    let mut total_len = self.before_query.len();
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
    if let Some(fragment) = &self.fragment {
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

    self.query = new_query;
    Ok(())
  }
}

fn try_clone_str(value: &str) -> Result<String, WebUrlError> {
  let mut out = String::new();
  out.try_reserve_exact(value.len())?;
  out.push_str(value);
  Ok(out)
}

