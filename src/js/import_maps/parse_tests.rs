use super::parse_import_map_string;
use super::{ImportMapError, ImportMapWarningKind};

use url::Url;

fn base_url() -> Url {
  Url::parse("https://example.com/base/page.html").unwrap()
}

#[test]
fn errors_when_top_level_is_not_object() {
  let err = parse_import_map_string("[]", &base_url()).unwrap_err();
  assert!(
    matches!(err, ImportMapError::TypeError(ref msg) if msg.contains("top-level value needs to be a JSON object")),
    "{err:?}"
  );
}

#[test]
fn errors_when_imports_is_not_object() {
  let err = parse_import_map_string(r#"{ "imports": [] }"#, &base_url()).unwrap_err();
  assert!(
    matches!(err, ImportMapError::TypeError(ref msg) if msg.contains(r#"value for the "imports" top-level key needs to be a JSON object"#)),
    "{err:?}"
  );
}

#[test]
fn normalizes_url_like_specifier_keys_to_absolute_url_strings() {
  let (map, _warnings) =
    parse_import_map_string(r#"{ "imports": { "/app/helper": "./helper.mjs" } }"#, &base_url())
      .unwrap();
  assert_eq!(map.imports.entries.len(), 1);
  assert_eq!(map.imports.entries[0].0, "https://example.com/app/helper");
}

#[test]
fn trailing_slash_mismatch_sets_address_to_null() {
  let (map, warnings) =
    parse_import_map_string(r#"{ "imports": { "pkg/": "/not-a-dir.js" } }"#, &base_url()).unwrap();
  assert_eq!(map.imports.entries, vec![("pkg/".to_string(), None)]);
  assert!(warnings.iter().any(|w| matches!(
    w.kind,
    ImportMapWarningKind::TrailingSlashMismatch { .. }
  )));
}

#[test]
fn non_string_addresses_become_null() {
  let (map, warnings) =
    parse_import_map_string(r#"{ "imports": { "foo": 123 } }"#, &base_url()).unwrap();
  assert_eq!(map.imports.entries, vec![("foo".to_string(), None)]);
  assert!(warnings.iter().any(|w| matches!(
    w.kind,
    ImportMapWarningKind::AddressNotString { .. }
  )));
}

#[test]
fn invalid_address_url_becomes_null() {
  let (map, warnings) = parse_import_map_string(
    r#"{ "imports": { "foo": "http://[::1" } }"#,
    &base_url(),
  )
  .unwrap();
  assert_eq!(map.imports.entries, vec![("foo".to_string(), None)]);
  assert!(warnings.iter().any(|w| matches!(
    w.kind,
    ImportMapWarningKind::AddressInvalid { .. }
  )));
}

#[test]
fn scope_prefix_parse_failure_is_ignored_with_warning() {
  let (map, warnings) = parse_import_map_string(
    r#"{ "scopes": { "http://[": { "foo": "/bar.js" } } }"#,
    &base_url(),
  )
  .unwrap();
  assert!(map.scopes.entries.is_empty(), "{map:?}");
  assert!(warnings.iter().any(|w| matches!(
    w.kind,
    ImportMapWarningKind::ScopePrefixNotParseable { .. }
  )));
}

#[test]
fn unknown_top_level_keys_warn_but_do_not_error() {
  let (_map, warnings) = parse_import_map_string(r#"{ "imports": {}, "bad": {} }"#, &base_url())
    .unwrap();
  assert!(warnings.iter().any(|w| matches!(
    &w.kind,
    ImportMapWarningKind::UnknownTopLevelKey { key } if key == "bad"
  )));
}

#[test]
fn bare_relative_integrity_keys_are_ignored() {
  let (map, warnings) = parse_import_map_string(
    r#"{ "integrity": { "foo": "sha256-abc" } }"#,
    &base_url(),
  )
  .unwrap();
  assert!(map.integrity.entries.is_empty());
  assert!(warnings.iter().any(|w| matches!(
    w.kind,
    ImportMapWarningKind::IntegrityKeyFailedToResolve { .. }
  )));
}

#[test]
fn imports_are_sorted_in_descending_code_unit_order() {
  let (map, _warnings) = parse_import_map_string(
    r#"{ "imports": { "foo/": "/dir/", "foo/bar/": "/dir2/" } }"#,
    &base_url(),
  )
  .unwrap();
  let keys: Vec<_> = map.imports.entries.iter().map(|(k, _)| k.as_str()).collect();
  assert_eq!(keys, vec!["foo/bar/", "foo/"]);
}
