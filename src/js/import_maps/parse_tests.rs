use super::{create_import_map_parse_result, parse_import_map_string, resolve_imports_match};
use super::{ImportMapError, ImportMapWarningKind};

use url::Url;

fn base_url() -> Url {
  Url::parse("https://example.com/base/page.html").unwrap()
}

#[test]
fn create_parse_result_captures_json_parse_errors() {
  let result = create_import_map_parse_result("{", &base_url());
  assert!(result.import_map.is_none());
  assert!(matches!(
    result.error_to_rethrow,
    Some(ImportMapError::Json(_))
  ));
}

#[test]
fn create_parse_result_captures_type_errors() {
  let result = create_import_map_parse_result("[]", &base_url());
  assert!(result.import_map.is_none());
  assert!(matches!(
    result.error_to_rethrow,
    Some(ImportMapError::TypeError(_))
  ));
}

#[test]
fn create_parse_result_includes_warnings_on_success() {
  let result = create_import_map_parse_result(r#"{ "imports": {}, "bad": {} }"#, &base_url());
  assert!(result.error_to_rethrow.is_none());
  assert!(result.import_map.is_some());
  assert!(result
    .warnings
    .iter()
    .any(|w| matches!(&w.kind, ImportMapWarningKind::UnknownTopLevelKey { key } if key == "bad")));
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
fn errors_when_scopes_is_not_object() {
  let err = parse_import_map_string(r#"{ "scopes": [] }"#, &base_url()).unwrap_err();
  assert!(
    matches!(err, ImportMapError::TypeError(ref msg) if msg.contains(r#"value for the "scopes" top-level key needs to be a JSON object"#)),
    "{err:?}"
  );
}

#[test]
fn errors_when_integrity_is_not_object() {
  let err = parse_import_map_string(r#"{ "integrity": [] }"#, &base_url()).unwrap_err();
  assert!(
    matches!(err, ImportMapError::TypeError(ref msg) if msg.contains(r#"value for the "integrity" top-level key needs to be a JSON object"#)),
    "{err:?}"
  );
}

#[test]
fn errors_when_scope_value_is_not_object() {
  let err = parse_import_map_string(r#"{ "scopes": { "/a/": [] } }"#, &base_url()).unwrap_err();
  assert!(
    matches!(err, ImportMapError::TypeError(ref msg) if msg.contains("prefix /a/")),
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
fn empty_specifier_key_is_skipped_with_warning() {
  let (map, warnings) =
    parse_import_map_string(r#"{ "imports": { "": "/skip.js", "a": "/a.js" } }"#, &base_url())
      .unwrap();
  assert_eq!(map.imports.entries.len(), 1);
  assert_eq!(map.imports.entries[0].0, "a");
  assert!(warnings
    .iter()
    .any(|w| matches!(w.kind, ImportMapWarningKind::EmptySpecifierKey)));
}

#[test]
fn matches_html_spec_normalization_example() {
  let (map, warnings) = parse_import_map_string(
    r#"{
  "imports": {
    "/app/helper": "./node_modules/helper/index.mjs",
    "lodash": "/node_modules/lodash-es/lodash.js"
  }
}"#,
    &base_url(),
  )
  .unwrap();

  assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

  let helper = map
    .imports
    .entries
    .iter()
    .find(|(k, _)| k == "https://example.com/app/helper")
    .expect("expected /app/helper entry");
  assert_eq!(
    helper.1.as_ref().expect("helper URL").as_str(),
    "https://example.com/base/node_modules/helper/index.mjs"
  );

  let lodash = map
    .imports
    .entries
    .iter()
    .find(|(k, _)| k == "lodash")
    .expect("expected lodash entry");
  assert_eq!(
    lodash.1.as_ref().expect("lodash URL").as_str(),
    "https://example.com/node_modules/lodash-es/lodash.js"
  );
}

#[test]
fn imports_normalization_collision_uses_input_key_order() {
  // Regression: import maps operate on Infra ordered maps, so JSON object insertion order must be
  // preserved when collisions occur during normalization.
  //
  // Use an ordering that differs from lexicographic sorting so this test would fail if parsing
  // materialized objects into a `BTreeMap`.
  let (map, _warnings) = parse_import_map_string(
    r#"{
      "imports": {
        "https://example.com/base/a": "/first.js",
        "./a": "/second.js"
      }
    }"#,
    &base_url(),
  )
  .unwrap();

  let entry = map
    .imports
    .entries
    .iter()
    .find(|(k, _)| k == "https://example.com/base/a")
    .expect("expected normalized /a entry");
  assert_eq!(
    entry.1.as_ref().expect("URL").as_str(),
    "https://example.com/second.js"
  );
}

#[test]
fn scopes_normalization_collision_uses_input_key_order() {
  let (map, _warnings) = parse_import_map_string(
    r#"{
      "scopes": {
        "https://example.com/scope/": { "x": "/first.js" },
        "/scope/": { "x": "/second.js" }
      }
    }"#,
    &base_url(),
  )
  .unwrap();

  let scope = map
    .scopes
    .entries
    .iter()
    .find(|(prefix, _)| prefix == "https://example.com/scope/")
    .expect("expected normalized scope");
  let entry = scope
    .1
    .entries
    .iter()
    .find(|(k, _)| k == "x")
    .expect("expected x mapping");
  assert_eq!(
    entry.1.as_ref().expect("URL").as_str(),
    "https://example.com/second.js"
  );
}

#[test]
fn bare_relative_addresses_become_null_with_warning() {
  let (map, warnings) =
    parse_import_map_string(r#"{ "imports": { "foo": "bar/baz.js" } }"#, &base_url()).unwrap();
  assert_eq!(map.imports.entries, vec![("foo".to_string(), None)]);
  assert!(warnings.iter().any(|w| matches!(
    &w.kind,
    ImportMapWarningKind::AddressInvalid { specifier_key, address }
      if specifier_key == "foo" && address == "bar/baz.js"
  )));
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
fn url_serialization_must_not_create_prefix_keys_with_non_slash_addresses() {
  // Edge case: URL serialization can add an implicit trailing slash. "https://example.com"
  // normalizes to "https://example.com/" (path '/'), which makes it a prefix key.
  //
  // Ensure the parser enforces the trailing-slash rule based on the *normalized* key string so we
  // never end up with a prefix key mapping to a non-slash URL (which would violate the resolver's
  // invariants).
  let base = Url::parse("https://example.com/app.html").unwrap();
  let (map, warnings) = parse_import_map_string(
    r#"{ "imports": { "https://example.com": "https://cdn.example.com/file.js" } }"#,
    &base,
  )
  .unwrap();

  assert_eq!(
    map.imports.entries,
    vec![("https://example.com/".to_string(), None)]
  );
  assert!(warnings.iter().any(|w| matches!(
    w.kind,
    ImportMapWarningKind::TrailingSlashMismatch { .. }
  )));

  // Most importantly: resolution must not hit the prefix invariant debug-assert.
  let specifier = "https://example.com/foo.js";
  let as_url = Url::parse(specifier).unwrap();
  let err = resolve_imports_match(specifier, Some(&as_url), &map.imports).unwrap_err();
  assert!(matches!(err, ImportMapError::TypeError(_)), "{err:?}");
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
fn integrity_values_must_be_strings() {
  let (map, warnings) = parse_import_map_string(
    r#"{ "integrity": { "/foo.js": 123 } }"#,
    &base_url(),
  )
  .unwrap();
  assert!(map.integrity.entries.is_empty());
  assert!(warnings.iter().any(|w| matches!(
    w.kind,
    ImportMapWarningKind::IntegrityValueNotString { .. }
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

#[test]
fn imports_sorting_uses_utf16_code_units_not_scalar_values() {
  // U+FFFF is a single UTF-16 code unit, while "💩" is represented as a surrogate pair.
  // HTML's import maps sorting is defined in terms of UTF-16 code units, so "a\uFFFF" should sort
  // after "a💩" in ascending order (and thus before it in descending order).
  let (map, _warnings) = parse_import_map_string(
    r#"{ "imports": { "a\uFFFF": "/x.js", "a💩": "/y.js" } }"#,
    &base_url(),
  )
  .unwrap();
  let keys: Vec<_> = map.imports.entries.iter().map(|(k, _)| k.as_str()).collect();
  assert_eq!(keys, vec!["a\u{FFFF}", "a💩"]);
}

fn read_fixture(rel_path: &str) -> String {
  let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel_path);
  std::fs::read_to_string(&fixture_path)
    .unwrap_or_else(|err| panic!("failed to read fixture {fixture_path:?}: {err}"))
}

fn extract_first_importmap_script_text(html: &str) -> String {
  let marker = r#"<script type="importmap""#;
  let start = html
    .find(marker)
    .unwrap_or_else(|| panic!("expected HTML fixture to contain {marker}"));
  let after_start = &html[start..];
  let open_tag_end = after_start
    .find('>')
    .unwrap_or_else(|| panic!("expected {marker} tag to have a closing '>'"))
    + start;
  let close_tag_start = html[open_tag_end + 1..]
    .find("</script>")
    .unwrap_or_else(|| panic!("expected importmap <script> to have a </script> closing tag"))
    + open_tag_end
    + 1;
  html[open_tag_end + 1..close_tag_start].trim().to_string()
}

#[test]
fn parses_wordpress_importmap_from_techcrunch_fixture() {
  let html = read_fixture("tests/pages/fixtures/techcrunch.com/index.html");
  let importmap_json = extract_first_importmap_script_text(&html);
  let base = Url::parse("https://techcrunch.com/").unwrap();

  let (map, warnings) = parse_import_map_string(&importmap_json, &base).unwrap();

  let addr = map
    .imports
    .entries
    .iter()
    .find(|(k, _)| k == "@wordpress/interactivity")
    .and_then(|(_, v)| v.as_ref())
    .expect("expected @wordpress/interactivity to resolve to a URL");
  assert!(
    addr
      .as_str()
      .starts_with("https://techcrunch.com/wp-includes/js/dist/script-modules/interactivity/"),
    "unexpected interactivity URL: {addr}"
  );

  // Warnings are allowed (real-world import maps may contain non-fatal oddities), but should not
  // prevent successful parsing.
  let _ = warnings;
}
