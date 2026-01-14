#![cfg(test)]

use super::{
  validate_and_extract_attribute, validate_and_extract_element, validate_attribute_local_name, DomError,
};

#[test]
fn element_local_name_rejects_lt_in_ascii_fast_path() {
  assert_eq!(
    validate_and_extract_element(None, "a<b"),
    Err(DomError::InvalidCharacterError)
  );
}

#[test]
fn element_qualified_name_rejects_lt_in_prefix_even_without_namespace() {
  // InvalidCharacterError should take precedence over NamespaceError.
  assert_eq!(
    validate_and_extract_element(None, "a<:b"),
    Err(DomError::InvalidCharacterError)
  );
}

#[test]
fn element_qualified_name_rejects_lt_in_local_name_even_without_namespace() {
  // InvalidCharacterError should take precedence over NamespaceError.
  assert_eq!(
    validate_and_extract_element(None, "x:y<z"),
    Err(DomError::InvalidCharacterError)
  );
}

#[test]
fn attribute_local_name_rejects_lt() {
  assert_eq!(
    validate_attribute_local_name("a<b"),
    Err(DomError::InvalidCharacterError)
  );
}

#[test]
fn attribute_qualified_name_rejects_lt_even_without_namespace() {
  // InvalidCharacterError should take precedence over NamespaceError.
  assert_eq!(
    validate_and_extract_attribute(None, "x:y<z"),
    Err(DomError::InvalidCharacterError)
  );
}

#[test]
fn attribute_qualified_name_rejects_lt_in_prefix_even_without_namespace() {
  // InvalidCharacterError should take precedence over NamespaceError.
  assert_eq!(
    validate_and_extract_attribute(None, "a<:b"),
    Err(DomError::InvalidCharacterError)
  );
}
