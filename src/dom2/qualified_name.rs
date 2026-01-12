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

fn is_xml_ncname_start_char(ch: char) -> bool {
  if ch == ':' {
    return false;
  }
  match ch {
    'A'..='Z' | '_' | 'a'..='z' => true,
    _ => {
      let u = ch as u32;
      matches!(u,
        0xC0..=0xD6
          | 0xD8..=0xF6
          | 0xF8..=0x2FF
          | 0x370..=0x37D
          | 0x37F..=0x1FFF
          | 0x200C..=0x200D
          | 0x2070..=0x218F
          | 0x2C00..=0x2FEF
          | 0x3001..=0xD7FF
          | 0xF900..=0xFDCF
          | 0xFDF0..=0xFFFD
          | 0x10000..=0xEFFFF)
    }
  }
}

fn is_xml_ncname_char(ch: char) -> bool {
  if is_xml_ncname_start_char(ch) {
    return true;
  }
  match ch {
    '-' | '.' | '0'..='9' | '\u{B7}' => true,
    _ => {
      let u = ch as u32;
      matches!(u, 0x0300..=0x036F | 0x203F..=0x2040)
    }
  }
}

fn is_valid_ncname(s: &str) -> bool {
  let mut chars = s.chars();
  let Some(first) = chars.next() else {
    return false;
  };
  if !is_xml_ncname_start_char(first) {
    return false;
  }
  for ch in chars {
    if !is_xml_ncname_char(ch) {
      return false;
    }
  }
  true
}

fn is_valid_qualified_name(s: &str) -> bool {
  let first_colon = s.find(':');
  let Some(first_colon) = first_colon else {
    return is_valid_ncname(s);
  };
  let Some(last_colon) = s.rfind(':') else {
    return false;
  };
  if first_colon != last_colon {
    // Multiple colons are not allowed in a QName.
    return false;
  }
  // Colon cannot be the first or last character.
  if first_colon == 0 || first_colon + 1 >= s.len() {
    return false;
  }
  let (prefix, local) = s.split_at(first_colon);
  let local = &local[1..];
  is_valid_ncname(prefix) && is_valid_ncname(local)
}

/// Parse `qualified_name` into `(prefix, local_name)` and validate it per the DOM Standard.
///
/// This implements the DOM Standard's "validate and extract" algorithm (but returns only the parsed
/// prefix/local name; callers keep the `namespace` separately).
///
/// It enforces:
/// - qualified name validity (`QName` per Namespaces in XML),
/// - the `xml` and `xmlns` namespace/prefix constraints.
pub fn validate_and_extract(
  namespace: Option<&str>,
  qualified_name: &str,
) -> Result<ParsedQualifiedName, DomError> {
  if !is_valid_qualified_name(qualified_name) {
    return Err(DomError::InvalidCharacterError);
  }

  let (prefix, local_name) = match qualified_name.split_once(':') {
    Some((prefix, local)) => (Some(prefix.to_string()), local.to_string()),
    None => (None, qualified_name.to_string()),
  };

  // Namespace/prefix constraints.
  if prefix.is_some() && namespace.is_none() {
    return Err(DomError::NamespaceError);
  }

  let prefix_str = prefix.as_deref();

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

  Ok(ParsedQualifiedName { prefix, local_name })
}
