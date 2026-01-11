use super::{
  create_import_map_parse_result, create_import_map_parse_result_with_limits, merge_existing_and_new_import_maps,
  merge_existing_and_new_import_maps_with_limits, parse_import_map_string, register_import_map,
  register_import_map_with_limits, resolve_module_integrity_metadata, resolve_module_specifier, ImportMap,
  ImportMapError, ImportMapLimits, ImportMapState, ModuleIntegrityMap, ModuleSpecifierMap, ScopesMap,
  ResolvedModuleSetIndex, SpecifierAsUrlKind, SpecifierResolutionRecord,
};

use super::merge::{merge_existing_and_new_import_maps_impl_instrumented, MergeStats};
use super::types::code_unit_cmp;
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
fn resolve_module_integrity_metadata_returns_entry_or_empty() {
  let base = Url::parse("https://example.com/page.html").unwrap();
  let mut state = ImportMapState::default();

  register_json(
    &mut state,
    r#"{
      "integrity": {
        "/a-1.mjs": "sha384-deadbeef"
      }
    }"#,
    &base,
  );

  let url = Url::parse("https://example.com/a-1.mjs").unwrap();
  assert_eq!(
    resolve_module_integrity_metadata(&state, &url),
    "sha384-deadbeef"
  );

  let missing = Url::parse("https://example.com/missing.mjs").unwrap();
  assert_eq!(resolve_module_integrity_metadata(&state, &missing), "");
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
  merge_existing_and_new_import_maps(&mut state, &new_scoped).unwrap();

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

  merge_existing_and_new_import_maps(&mut state, &new_scoped).unwrap();

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
fn scope_filtering_handles_unsorted_resolved_module_set_base_url_index() {
  // `ResolvedModuleSetIndex` is normally populated incrementally as modules are resolved. Ensure
  // scope-prefix filtering still works when the base URLs are inserted in non-sorted order.
  let mut state = ImportMapState::default();
  state.resolved_module_set.push_record(SpecifierResolutionRecord {
    serialized_base_url: Some("https://example.com/b/main.js".to_string()),
    specifier: "unrelated".to_string(),
    as_url_kind: SpecifierAsUrlKind::NotUrl,
  });
  state.resolved_module_set.push_record(SpecifierResolutionRecord {
    serialized_base_url: Some("https://example.com/a/main.js".to_string()),
    specifier: "bar".to_string(),
    as_url_kind: SpecifierAsUrlKind::NotUrl,
  });

  let new_scoped = parse_map(
    r#"{
      "scopes": {
        "/a/": {
          "bar": "https://cdn.example/bar.js",
          "keep": "https://cdn.example/keep.js"
        }
      }
    }"#,
    "https://example.com/index.html",
  );

  merge_existing_and_new_import_maps(&mut state, &new_scoped).unwrap();

  let scope = state
    .import_map
    .scopes
    .get("https://example.com/a/")
    .expect("scope inserted");
  assert!(
    !scope.contains_key("bar"),
    "expected exact-match rule to be removed due to resolved module set"
  );
  assert!(scope.contains_key("keep"));
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

  merge_existing_and_new_import_maps(&mut state, &scoped).unwrap();

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

  let a = Url::parse("https://example.com/a.js").unwrap();
  let b = Url::parse("https://example.com/b.js").unwrap();
  assert_eq!(resolve_module_integrity_metadata(&state, &a), "sha256-old");
  assert_eq!(resolve_module_integrity_metadata(&state, &b), "sha256-b");
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
fn prefix_mapping_handles_non_ascii_bare_specifiers_with_non_bmp_chars() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  // 💩 is outside the BMP (two UTF-16 code units); ensure prefix candidate enumeration and slicing
  // are correct and do not panic.
  register_json(
    &mut state,
    r#"{
      "imports": {
        "pkg/💩/": "https://cdn.example/pile/",
        "pkg/": "https://cdn.example/pkg/"
      }
    }"#,
    &base,
  );

  let resolved = resolve_module_specifier(&mut state, "pkg/💩/mod.js", &base).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/pile/mod.js");
}

#[test]
fn prefix_mapping_handles_url_serialization_with_non_ascii_input() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  // The key and specifier contain a non-BMP character; URL serialization percent-encodes it.
  register_json(
    &mut state,
    r#"{
      "imports": {
        "https://example.test/💩/": "https://cdn.example/pile/"
      }
    }"#,
    &base,
  );

  let resolved =
    resolve_module_specifier(&mut state, "https://example.test/💩/mod.js", &base).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/pile/mod.js");
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

#[test]
fn parse_import_map_integrity_entries_are_stored() {
  let base_url = Url::parse("https://example.com/base/page.html").unwrap();
  let (import_map, warnings) = parse_import_map_string(
    r#"{
      "integrity": {
        "./a.js": "sha256-aaa",
        "/b.js": "sha384-bbb",
        "https://example.com/c.js": "sha512-ccc"
      }
    }"#,
    &base_url,
  )
  .unwrap();

  assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
  assert_eq!(
    import_map.integrity.entries,
    vec![
      ("https://example.com/base/a.js".to_string(), "sha256-aaa".to_string()),
      ("https://example.com/b.js".to_string(), "sha384-bbb".to_string()),
      ("https://example.com/c.js".to_string(), "sha512-ccc".to_string()),
    ]
  );
}

#[test]
fn resolve_module_integrity_metadata_returns_match_or_empty() {
  let base_url = Url::parse("https://example.com/").unwrap();
  let (import_map, _warnings) = parse_import_map_string(
    r#"{
      "integrity": {
        "./a.js": "sha256-aaa"
      }
    }"#,
    &base_url,
  )
  .unwrap();

  let state = ImportMapState {
    import_map,
    resolved_module_set: ResolvedModuleSetIndex::default(),
  };

  let a = Url::parse("https://example.com/a.js").unwrap();
  assert_eq!(
    resolve_module_integrity_metadata(&state, &a),
    "sha256-aaa"
  );

  let missing = Url::parse("https://example.com/missing.js").unwrap();
  assert_eq!(resolve_module_integrity_metadata(&state, &missing), "");
}

#[test]
fn merge_integrity_ignores_duplicates() {
  let base_url = Url::parse("https://example.com/").unwrap();
  let (first, _warnings) = parse_import_map_string(
    r#"{
      "integrity": {
        "./a.js": "sha256-first"
      }
    }"#,
    &base_url,
  )
  .unwrap();
  let (second, _warnings) = parse_import_map_string(
    r#"{
      "integrity": {
        "./a.js": "sha256-second",
        "./b.js": "sha256-b"
      }
    }"#,
    &base_url,
  )
  .unwrap();

  let mut state = ImportMapState::default();
  merge_existing_and_new_import_maps(&mut state, &first).unwrap();
  merge_existing_and_new_import_maps(&mut state, &second).unwrap();

  let a = Url::parse("https://example.com/a.js").unwrap();
  assert_eq!(
    resolve_module_integrity_metadata(&state, &a),
    "sha256-first"
  );

  let b = Url::parse("https://example.com/b.js").unwrap();
  assert_eq!(resolve_module_integrity_metadata(&state, &b), "sha256-b");
}

#[test]
fn merge_filters_large_resolved_module_set_without_quadratic_scans() {
  // This test constructs a moderately large resolved module set and import map to ensure the merge
  // algorithm stays correct, and to make accidental O(R * N) regressions noticeable in CI.
  const N: usize = 5_000;

  let base_url = "https://example.com/app/main.js".to_string();
  let scope_prefix = "https://example.com/app/".to_string();

  // Build the resolved module set as a batch so we don't benchmark index maintenance here.
  let mut resolved_records: Vec<SpecifierResolutionRecord> = Vec::with_capacity(N * 2 + 1);

  // Resolved module set entries that will block new top-level imports whose key starts with
  // `imp{i}`.
  for i in 0..N {
    resolved_records.push(SpecifierResolutionRecord {
      serialized_base_url: Some(base_url.clone()),
      specifier: format!("imp{i}"),
      as_url_kind: SpecifierAsUrlKind::NotUrl,
    });
  }

  // Resolved module set entries that will block scoped rules with keys `sc{i}/` (prefix) and
  // `sc{i}/sub` (exact).
  for i in 0..N {
    resolved_records.push(SpecifierResolutionRecord {
      serialized_base_url: Some(base_url.clone()),
      specifier: format!("sc{i}/sub"),
      as_url_kind: SpecifierAsUrlKind::NotUrl,
    });
  }

  // Non-special URL-like resolved specifier: should *not* cause scoped prefix filtering.
  resolved_records.push(SpecifierResolutionRecord {
    serialized_base_url: Some(base_url.clone()),
    specifier: "blob:https://example.com/uuid".to_string(),
    as_url_kind: SpecifierAsUrlKind::NonSpecial,
  });

  let mut state = ImportMapState {
    resolved_module_set: ResolvedModuleSetIndex::from_records(resolved_records),
    ..Default::default()
  };

  let mut new_imports = Vec::with_capacity(N * 3);
  for i in 0..N {
    // Filtered: starts with resolved `imp{i}`.
    new_imports.push((format!("imp{i}"), None));
    new_imports.push((format!("imp{i}/x"), None));
    // Retained: no resolved specifier is a prefix of this key.
    new_imports.push((format!("keep{i}"), None));
  }

  let mut new_scope_imports = Vec::with_capacity(N * 3 + 1);
  for i in 0..N {
    // Filtered due to resolved `sc{i}/sub` within this scope.
    new_scope_imports.push((format!("sc{i}/"), None));
    new_scope_imports.push((format!("sc{i}/sub"), None));
    // Retained.
    new_scope_imports.push((format!("keep_sc{i}"), None));
  }
  new_scope_imports.push(("blob:https://example.com/".to_string(), None));
  new_scope_imports.sort_by(|(a, _), (b, _)| code_unit_cmp(b.as_str(), a.as_str()));

  let new_import_map = ImportMap {
    imports: ModuleSpecifierMap { entries: new_imports },
    scopes: ScopesMap {
      entries: vec![(
        scope_prefix.clone(),
        ModuleSpecifierMap {
          entries: new_scope_imports,
        },
      )],
    },
    integrity: ModuleIntegrityMap::default(),
  };

  // This test uses a large synthetic import map, so use larger limits explicitly.
  let limits = ImportMapLimits {
    max_total_entries: 100_000,
    max_imports_entries: 100_000,
    max_scopes: 10,
    max_scope_entries: 100_000,
    max_integrity_entries: 0,
    ..ImportMapLimits::default()
  };
  merge_existing_and_new_import_maps_with_limits(&mut state, &new_import_map, &limits).unwrap();

  assert!(!state.import_map.imports.contains_key("imp0"));
  assert!(!state.import_map.imports.contains_key("imp0/x"));
  assert!(state.import_map.imports.contains_key("keep0"));

  let last_imp = format!("imp{}", N - 1);
  let last_keep = format!("keep{}", N - 1);
  assert!(!state.import_map.imports.contains_key(last_imp.as_str()));
  assert!(state.import_map.imports.contains_key(last_keep.as_str()));

  let merged_scope = state
    .import_map
    .scopes
    .get(scope_prefix.as_str())
    .expect("scope exists after merge");

  assert!(!merged_scope.contains_key("sc0/"));
  assert!(!merged_scope.contains_key("sc0/sub"));
  assert!(merged_scope.contains_key("keep_sc0"));

  assert!(
    merged_scope.contains_key("blob:https://example.com/"),
    "expected prefix rule to remain for non-special URL-like resolved specifier"
  );
}

#[test]
fn merge_large_resolved_module_set_is_linearish() {
  // Regression test for the HTML spec's note that resolved-module-set filtering should avoid naive
  // nested scans when the resolved module set grows large.
  let n: usize = 5_000;

  let mut records = Vec::with_capacity(n);
  for i in 0..n {
    records.push(SpecifierResolutionRecord {
      serialized_base_url: Some(format!("https://example.com/scope/{i}.mjs")),
      specifier: format!("pkg{i}/module"),
      as_url_kind: SpecifierAsUrlKind::NotUrl,
    });
  }

  let mut state = ImportMapState {
    import_map: ImportMap::default(),
    resolved_module_set: ResolvedModuleSetIndex::from_records(records),
  };

  let mut scope_imports = ModuleSpecifierMap {
    entries: (0..n)
      .map(|i| {
        (
          format!("pkg{i}/"),
          Some(Url::parse(&format!("https://cdn.example.com/pkg{i}/")).unwrap()),
        )
      })
      .collect(),
  };
  scope_imports
    .entries
    .sort_by(|(a, _), (b, _)| code_unit_cmp(b.as_str(), a.as_str()));

  let mut imports = ModuleSpecifierMap {
    entries: (0..n)
      .map(|i| {
        (
          format!("pkg{i}/module/sub"),
          Some(Url::parse(&format!("https://cdn.example.com/pkg{i}/module/sub.mjs")).unwrap()),
        )
      })
      .collect(),
  };
  imports
    .entries
    .sort_by(|(a, _), (b, _)| code_unit_cmp(b.as_str(), a.as_str()));

  let new_import_map = ImportMap {
    imports,
    scopes: ScopesMap {
      entries: vec![("https://example.com/scope/".to_string(), scope_imports)],
    },
    integrity: ModuleIntegrityMap::default(),
  };

  let mut stats = MergeStats::default();
  merge_existing_and_new_import_maps_impl_instrumented(
    &mut state.import_map,
    &state.resolved_module_set,
    &new_import_map,
    &mut stats,
  );

  // The merge should not do per-record-per-key work.
  assert!(
    stats.scope_records_scanned <= n + 10,
    "unexpected number of scope record scans: {stats:?}"
  );
  assert!(
    stats.scope_keys_checked <= n + 10,
    "unexpected number of scope key checks: {stats:?}"
  );
  assert!(
    stats.top_level_import_keys_checked <= n + 10,
    "unexpected number of top-level import key checks: {stats:?}"
  );
}

#[test]
fn registering_many_import_maps_is_bounded_by_merge_limits() {
  let base = Url::parse("https://example.com/app/page.html").unwrap();
  let mut state = ImportMapState::default();

  let limits = ImportMapLimits {
    max_total_entries: 2,
    max_imports_entries: 2,
    // No scopes/integrity in this test.
    max_scopes: 0,
    max_scope_entries: 0,
    max_integrity_entries: 0,
    ..ImportMapLimits::default()
  };

  for key in ["a", "b"] {
    let json = format!(r#"{{ "imports": {{ "{key}": "/{key}.js" }} }}"#);
    let result = create_import_map_parse_result_with_limits(&json, &base, &limits);
    register_import_map_with_limits(&mut state, result, &limits).unwrap();
  }

  assert_eq!(state.import_map.imports.len(), 2);

  let third = create_import_map_parse_result_with_limits(r#"{ "imports": { "c": "/c.js" } }"#, &base, &limits);
  let err = register_import_map_with_limits(&mut state, third, &limits).unwrap_err();
  assert!(matches!(err, ImportMapError::LimitExceeded(_)), "{err:?}");

  // The merge should fail without partially mutating state.
  assert!(state.import_map.imports.contains_key("a"));
  assert!(state.import_map.imports.contains_key("b"));
  assert!(!state.import_map.imports.contains_key("c"));
  assert_eq!(state.import_map.imports.len(), 2);
}
