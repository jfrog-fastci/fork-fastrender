use super::DomError;

/// The XML namespace (`xml` prefix).
///
/// https://www.w3.org/TR/xml-names/#ns-decl
pub const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The XMLNS namespace (`xmlns` prefix).
///
/// https://www.w3.org/TR/xml-names/#ns-decl
pub const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQualifiedName {
  pub prefix: Option<String>,
  pub local_name: String,
}

#[inline]
fn is_dom_ascii_whitespace(byte: u8) -> bool {
  // DOM "ASCII whitespace" excludes U+000B (vertical tab).
  matches!(byte, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn is_valid_namespace_prefix(prefix: &str) -> bool {
  if prefix.is_empty() {
    return false;
  }
  !prefix.bytes().any(|b| {
    is_dom_ascii_whitespace(b) || b == b'\0' || b == b'<' || b == b'/' || b == b'>'
  })
}

fn is_valid_attribute_local_name(name: &str) -> bool {
  if name.is_empty() {
    return false;
  }
  !name.bytes().any(|b| {
    is_dom_ascii_whitespace(b)
      || b == b'\0'
      || b == b'<'
      || b == b'/'
      || b == b'='
      || b == b'>'
  })
}

fn is_valid_element_local_name(name: &str) -> bool {
  let mut chars = name.chars();
  let Some(first) = chars.next() else {
    return false;
  };

  // https://dom.spec.whatwg.org/#valid-element-local-name
  if first.is_ascii_alphabetic() {
    return !name.bytes().any(|b| {
      is_dom_ascii_whitespace(b)
        || b == b'\0'
        || b == b'<'
        || b == b'/'
        || b == b'>'
    });
  }

  // https://dom.spec.whatwg.org/#valid-element-local-name
  if !(first == ':' || first == '_' || (first as u32) >= 0x80) {
    return false;
  }

  for ch in chars {
    if ch.is_ascii_alphanumeric()
      || ch == '-'
      || ch == '.'
      || ch == ':'
      || ch == '_'
      || (ch as u32) >= 0x80
    {
      continue;
    }
    return false;
  }

  true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameContext {
  Element,
  Attribute,
}

fn split_qualified_name(qualified_name: &str) -> Result<(Option<&str>, &str), DomError> {
  match qualified_name.find(':') {
    Some(first_colon) => {
      let last_colon = qualified_name.rfind(':').unwrap_or(first_colon);
      if first_colon != last_colon {
        return Err(DomError::InvalidCharacterError);
      }
      let (prefix, local) = qualified_name.split_at(first_colon);
      // SAFETY: `split_at` splits on a UTF-8 boundary, and `:` is a single-byte ASCII character.
      let local = &local[1..];
      Ok((Some(prefix), local))
    }
    None => Ok((None, qualified_name)),
  }
}

fn validate_and_extract_with_context(
  namespace: Option<&str>,
  qualified_name: &str,
  context: NameContext,
) -> Result<ParsedQualifiedName, DomError> {
  let (prefix, local_name) = split_qualified_name(qualified_name)?;
  if let Some(prefix) = prefix {
    if !is_valid_namespace_prefix(prefix) {
      return Err(DomError::InvalidCharacterError);
    }
  }

  let local_name_ok = match context {
    NameContext::Element => is_valid_element_local_name(local_name),
    NameContext::Attribute => is_valid_attribute_local_name(local_name),
  };
  if !local_name_ok {
    return Err(DomError::InvalidCharacterError);
  }

  // Namespace/prefix constraints.
  if prefix.is_some() && namespace.is_none() {
    return Err(DomError::NamespaceError);
  }

  let prefix_str = prefix;

  // `xml` prefix is reserved for the XML namespace.
  if prefix_str == Some("xml") && namespace != Some(XML_NAMESPACE) {
    return Err(DomError::NamespaceError);
  }

  // XMLNS namespace rules.
  //
  // - If qualifiedName is `xmlns` or has an `xmlns` prefix, namespace must be XMLNS_NAMESPACE.
  // - If namespace is XMLNS_NAMESPACE, qualifiedName must be `xmlns` or have an `xmlns` prefix.
  let is_xmlns_name = qualified_name == "xmlns" || prefix_str == Some("xmlns");
  if is_xmlns_name && namespace != Some(XMLNS_NAMESPACE) {
    return Err(DomError::NamespaceError);
  }
  if namespace == Some(XMLNS_NAMESPACE) && !is_xmlns_name {
    return Err(DomError::NamespaceError);
  }

  Ok(ParsedQualifiedName {
    prefix: prefix.map(|p| p.to_string()),
    local_name: local_name.to_string(),
  })
}

/// Validate and extract a namespace + qualified name for an element (used by `createElementNS()`).
pub fn validate_and_extract_element(
  namespace: Option<&str>,
  qualified_name: &str,
) -> Result<ParsedQualifiedName, DomError> {
  validate_and_extract_with_context(namespace, qualified_name, NameContext::Element)
}

/// Validate and extract a namespace + qualified name for an attribute (used by `createAttributeNS()`).
pub fn validate_and_extract_attribute(
  namespace: Option<&str>,
  qualified_name: &str,
) -> Result<ParsedQualifiedName, DomError> {
  validate_and_extract_with_context(namespace, qualified_name, NameContext::Attribute)
}

/// Validate an attribute local name (used by `createAttribute()`).
pub fn validate_attribute_local_name(local_name: &str) -> Result<(), DomError> {
  if is_valid_attribute_local_name(local_name) {
    Ok(())
  } else {
    Err(DomError::InvalidCharacterError)
  }
}

fn validate_qualified_name_with_context(
  qualified_name: &str,
  context: NameContext,
) -> Result<(), DomError> {
  let (prefix, local_name) = split_qualified_name(qualified_name)?;
  if let Some(prefix) = prefix {
    if !is_valid_namespace_prefix(prefix) {
      return Err(DomError::InvalidCharacterError);
    }
  }

  let local_ok = match context {
    NameContext::Element => is_valid_element_local_name(local_name),
    NameContext::Attribute => is_valid_attribute_local_name(local_name),
  };
  if !local_ok {
    return Err(DomError::InvalidCharacterError);
  }

  Ok(())
}

/// Validate a qualified name for an element in non-namespace DOM APIs (e.g. `Document.createElement`).
pub fn validate_element_qualified_name(qualified_name: &str) -> Result<(), DomError> {
  validate_qualified_name_with_context(qualified_name, NameContext::Element)
}

/// Validate a qualified name for an attribute in non-namespace DOM APIs (e.g. `Element.setAttribute`).
pub fn validate_attribute_qualified_name(qualified_name: &str) -> Result<(), DomError> {
  validate_qualified_name_with_context(qualified_name, NameContext::Attribute)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validate_element_qualified_name_rejects_curated_invalid_cases() {
    for name in ["", "a b", "a<b", "a:b:c"] {
      assert_eq!(
        validate_element_qualified_name(name).unwrap_err(),
        DomError::InvalidCharacterError,
        "expected InvalidCharacterError for element qualified name {name:?}"
      );
    }
  }

  #[test]
  fn validate_attribute_qualified_name_rejects_curated_invalid_cases() {
    for name in ["", "a b", "a<b", "a:b:c"] {
      assert_eq!(
        validate_attribute_qualified_name(name).unwrap_err(),
        DomError::InvalidCharacterError,
        "expected InvalidCharacterError for attribute qualified name {name:?}"
      );
    }
  }

  #[test]
  fn validate_qualified_names_accept_common_valid_names() {
    for name in ["div", "my-element", "a:b", "x_y"] {
      assert!(
        validate_element_qualified_name(name).is_ok(),
        "expected element qualified name {name:?} to be valid"
      );
    }

    for name in ["id", "class", "data-foo", "aria-label", "a:b"] {
      assert!(
        validate_attribute_qualified_name(name).is_ok(),
        "expected attribute qualified name {name:?} to be valid"
      );
    }
  }

  #[test]
  fn rejects_less_than_in_namespace_prefix() {
    assert_eq!(
      validate_element_qualified_name("a<:b").unwrap_err(),
      DomError::InvalidCharacterError
    );
  }

  #[test]
  fn rejects_less_than_in_prefixed_local_name_for_attributes() {
    assert_eq!(
      validate_attribute_qualified_name("a:b<").unwrap_err(),
      DomError::InvalidCharacterError
    );
  }
}
