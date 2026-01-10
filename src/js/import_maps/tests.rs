use super::{
  create_import_map_parse_result, merge_existing_and_new_import_maps, register_import_map, resolve_module_specifier,
  ImportMap, ImportMapError, ImportMapState, SpecifierAsUrlKind,
};

use url::Url;

fn parse_map(json: &str, base: &str) -> ImportMap {
  let base_url = Url::parse(base).expect("base URL");
  let result = create_import_map_parse_result(json, &base_url);
  assert!(
    result.error_to_rethrow.is_none(),
    "unexpected parse error: {:?}",
    result.error_to_rethrow
  );
  result.import_map.expect("import map")
}

fn register_json(state: &mut ImportMapState, json: &str, base: &Url) {
  let result = create_import_map_parse_result(json, base);
  register_import_map(state, result).unwrap();
}

#[test]
fn conflicting_rules_are_ignored() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();

  let mut state = ImportMapState::default();

  // First import map defines /app/helper.
  register_json(
    &mut state,
    r#"{
      "imports": { "/app/helper": "/v1/helper.js" }
    }"#,
    &base,
  );

  // Second import map tries to redefine /app/helper and also adds /app/extra.
  register_json(
    &mut state,
    r#"{
      "imports": {
        "/app/helper": "/v2/helper.js",
        "/app/extra": "/v1/extra.js"
      }
    }"#,
    &base,
  );

  let resolved_helper = resolve_module_specifier(&mut state, "/app/helper", &base).unwrap();
  assert_eq!(resolved_helper.as_str(), "https://example.com/v1/helper.js");

  let resolved_extra = resolve_module_specifier(&mut state, "/app/extra", &base).unwrap();
  assert_eq!(resolved_extra.as_str(), "https://example.com/v1/extra.js");
}

#[test]
fn conflicting_scoped_rules_are_ignored() {
  let base = Url::parse("https://example.com/index.html").unwrap();
  let mut state = ImportMapState::default();

  // Existing scoped rule.
  register_json(
    &mut state,
    r#"{
      "scopes": {
        "/app/": {
          "foo": "https://cdn.example/foo-v1.js"
        }
      }
    }"#,
    &base,
  );

  // New import map tries to redefine foo and also add bar.
  register_json(
    &mut state,
    r#"{
      "scopes": {
        "/app/": {
          "foo": "https://cdn.example/foo-v2.js",
          "bar": "https://cdn.example/bar.js"
        }
      }
    }"#,
    &base,
  );

  let scope = state
    .import_map
    .scopes
    .get("https://example.com/app/")
    .expect("scope exists");

  assert_eq!(
    scope
      .get("foo")
      .unwrap()
      .as_ref()
      .unwrap()
      .as_str(),
    "https://cdn.example/foo-v1.js"
  );
  assert_eq!(
    scope
      .get("bar")
      .unwrap()
      .as_ref()
      .unwrap()
      .as_str(),
    "https://cdn.example/bar.js"
  );
}

#[test]
fn rules_impacting_already_resolved_modules_are_removed_from_imports() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "imports": { "lodash": "https://cdn.example/lodash-v1.js" }
    }"#,
    &base,
  );

  // Resolve lodash so it enters the resolved module set.
  let resolved = resolve_module_specifier(&mut state, "lodash", &base).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/lodash-v1.js");

  // Second import map attempts to redefine lodash and add lodash/ prefix mapping. Both should be
  // filtered out due to the resolved module set.
  register_json(
    &mut state,
    r#"{
      "imports": {
        "lodash": "https://cdn.example/lodash-v2.js",
        "lodash/": "https://cdn.example/lodash/"
      }
    }"#,
    &base,
  );

  let resolved_again = resolve_module_specifier(&mut state, "lodash", &base).unwrap();
  assert_eq!(resolved_again.as_str(), "https://cdn.example/lodash-v1.js");

  assert_eq!(
    state
      .import_map
      .imports
      .get("lodash")
      .unwrap()
      .as_ref()
      .unwrap()
      .as_str(),
    "https://cdn.example/lodash-v1.js"
  );
  assert!(
    !state.import_map.imports.contains_key("lodash/"),
    "expected lodash/ prefix mapping to be filtered out"
  );
}

#[test]
fn scope_filtering_matches_on_serialized_base_url_prefix() {
  let base = "https://example.com/app/main.js";
  let base_url = Url::parse(base).unwrap();

  let mut state = ImportMapState::default();
  register_json(
    &mut state,
    r#"{
      "imports": { "pkg/sub": "https://cdn.example/pkg/sub.js" }
    }"#,
    &base_url,
  );

  // Resolve a module from within /app/ so it enters the resolved module set.
  resolve_module_specifier(&mut state, "pkg/sub", &base_url).unwrap();

  let new_scoped = parse_map(
    r#"{
      "scopes": {
        "/app/": {
          "pkg/": "https://cdn.example/pkg/",
          "other": "https://cdn.example/other.js"
        }
      }
    }"#,
    base,
  );

  // Merge: the pkg/ rule would impact the already-resolved pkg/sub and must be removed, while the
  // unrelated "other" rule remains.
  merge_existing_and_new_import_maps(&mut state, &new_scoped);

  let scope = state
    .import_map
    .scopes
    .get("https://example.com/app/")
    .expect("scope inserted");
  assert!(
    !scope.contains_key("pkg/"),
    "expected pkg/ rule to be removed due to resolved module set"
  );
  assert!(scope.contains_key("other"));
}

#[test]
fn scope_filtering_does_not_apply_when_base_url_does_not_match_scope_prefix() {
  let base = "https://example.com/app/main.js";
  let base_url = Url::parse(base).unwrap();

  let mut state = ImportMapState::default();
  register_json(
    &mut state,
    r#"{
      "imports": { "pkg/sub": "https://cdn.example/pkg/sub.js" }
    }"#,
    &base_url,
  );
  resolve_module_specifier(&mut state, "pkg/sub", &base_url).unwrap();

  // Scope prefix does not match the base URL, so no filtering should occur.
  let new_scoped = parse_map(
    r#"{
      "scopes": {
        "/other/": {
          "pkg/": "https://cdn.example/pkg/"
        }
      }
    }"#,
    base,
  );

  merge_existing_and_new_import_maps(&mut state, &new_scoped);

  let scope = state
    .import_map
    .scopes
    .get("https://example.com/other/")
    .expect("scope inserted");
  assert!(
    scope.contains_key("pkg/"),
    "expected pkg/ rule to remain since scope prefix did not match resolved base URL"
  );
}

#[test]
fn non_special_url_like_specifiers_do_not_trigger_scope_prefix_filtering() {
  let base_url = Url::parse("https://example.com/app/main.js").unwrap();
  let mut state = ImportMapState::default();

  // Resolve a non-special URL-like specifier (blob: is non-special).
  let blob_specifier = "blob:https://example.com/uuid";
  let resolved = resolve_module_specifier(&mut state, blob_specifier, &base_url).unwrap();
  assert_eq!(resolved.as_str(), blob_specifier);

  let scoped = parse_map(
    r#"{
      "scopes": {
        "/app/": {
          "blob:https://example.com/": "https://cdn.example/blob-prefix/"
        }
      }
    }"#,
    base_url.as_str(),
  );

  merge_existing_and_new_import_maps(&mut state, &scoped);

  let scope = state
    .import_map
    .scopes
    .get("https://example.com/app/")
    .expect("scope inserted");
  assert!(
    scope.contains_key("blob:https://example.com/"),
    "expected prefix rule to remain for non-special URL-like resolved specifier"
  );
}

#[test]
fn integrity_merge_ignores_duplicates() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "integrity": {
        "/a.js": "sha256-old"
      }
    }"#,
    &base,
  );

  register_json(
    &mut state,
    r#"{
      "integrity": {
        "/a.js": "sha256-new",
        "/b.js": "sha256-b"
      }
    }"#,
    &base,
  );

  assert_eq!(
    state
      .import_map
      .integrity
      .get("https://example.com/a.js")
      .unwrap(),
    "sha256-old"
  );
  assert_eq!(
    state
      .import_map
      .integrity
      .get("https://example.com/b.js")
      .unwrap(),
    "sha256-b"
  );
}

#[test]
fn url_like_specifier_resolves_without_import_map() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  let resolved = resolve_module_specifier(&mut state, "./dep.js", &base).unwrap();
  assert_eq!(resolved.as_str(), "https://example.com/app/dep.js");

  assert_eq!(state.resolved_module_set.len(), 1);
  let record = state.resolved_module_set.last().unwrap();
  assert_eq!(record.serialized_base_url.as_deref(), Some(base.as_str()));
  assert_eq!(record.specifier, "https://example.com/app/dep.js");
  assert_eq!(record.as_url_kind, SpecifierAsUrlKind::Special);
}

#[test]
fn bare_specifier_with_no_mapping_errors() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  let err = resolve_module_specifier(&mut state, "lodash", &base).unwrap_err();
  assert!(matches!(err, ImportMapError::TypeError(_)), "{err:?}");
  assert!(state.resolved_module_set.is_empty());
}

#[test]
fn exact_match_mapping_works() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "imports": {
        "lodash": "https://cdn.example/lodash.js"
      }
    }"#,
    &base,
  );

  let resolved = resolve_module_specifier(&mut state, "lodash", &base).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/lodash.js");

  let record = state.resolved_module_set.last().unwrap();
  assert_eq!(record.serialized_base_url.as_deref(), Some(base.as_str()));
  assert_eq!(record.specifier, "lodash");
  assert_eq!(record.as_url_kind, SpecifierAsUrlKind::NotUrl);
}

#[test]
fn prefix_mapping_chooses_longest_prefix() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "imports": {
        "pkg/": "https://cdn.example/pkg/",
        "pkg/sub/": "https://cdn.example/pkg-sub/"
      }
    }"#,
    &base,
  );

  let resolved = resolve_module_specifier(&mut state, "pkg/sub/mod.js", &base).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/pkg-sub/mod.js");

  let record = state.resolved_module_set.last().unwrap();
  assert_eq!(record.specifier, "pkg/sub/mod.js");
  assert_eq!(record.as_url_kind, SpecifierAsUrlKind::NotUrl);
}

#[test]
fn prefix_mapping_backtracking_throws() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "imports": {
        "pkg/": "https://cdn.example/pkg/"
      }
    }"#,
    &base,
  );

  let err = resolve_module_specifier(&mut state, "pkg/../evil.js", &base).unwrap_err();
  assert!(matches!(err, ImportMapError::TypeError(_)), "{err:?}");
  assert!(state.resolved_module_set.is_empty());
}

#[test]
fn null_entries_throw_and_prevent_fallback() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  // The null address blocks resolution (the resolver must throw and not fall back to the URL-like
  // specifier's direct URL).
  register_json(
    &mut state,
    r#"{
      "imports": {
        "./dep.js": null
      }
    }"#,
    &base,
  );

  let err = resolve_module_specifier(&mut state, "./dep.js", &base).unwrap_err();
  assert!(matches!(err, ImportMapError::TypeError(_)), "{err:?}");
  assert!(state.resolved_module_set.is_empty());
}

#[test]
fn scopes_override_imports_when_base_url_matches() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "imports": {
        "foo": "/imports/foo.js"
      },
      "scopes": {
        "/app/": {
          "foo": "/scopes/foo.js"
        }
      }
    }"#,
    &base,
  );

  let resolved = resolve_module_specifier(&mut state, "foo", &base).unwrap();
  assert_eq!(resolved.as_str(), "https://example.com/scopes/foo.js");
}

#[test]
fn as_url_non_special_disables_prefix_mapping() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "imports": {
        "blob:https://example.com/": "https://cdn.example/blob/"
      }
    }"#,
    &base,
  );

  let specifier = "blob:https://example.com/uuid";
  let resolved = resolve_module_specifier(&mut state, specifier, &base).unwrap();
  assert_eq!(resolved.as_str(), specifier);

  let record = state.resolved_module_set.last().unwrap();
  assert_eq!(record.specifier, Url::parse(specifier).unwrap().to_string());
  assert_eq!(record.as_url_kind, SpecifierAsUrlKind::NonSpecial);
}
