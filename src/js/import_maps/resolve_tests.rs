use url::Url;

use super::{
  resolve_module_specifier, ImportMap, ImportMapError, ImportMapState, ModuleIntegrityMap,
  ModuleSpecifierMap, ScopesMap,
};

fn url(input: &str) -> Url {
  Url::parse(input).unwrap()
}

fn module_specifier_map(entries: Vec<(&str, Option<&str>)>) -> ModuleSpecifierMap {
  let mut entries: Vec<(String, Option<Url>)> = entries
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.map(url)))
    .collect();
  entries.sort_by(|(a, _), (b, _)| super::types::code_unit_cmp(b, a));
  ModuleSpecifierMap { entries }
}

fn scopes_map(entries: Vec<(&str, ModuleSpecifierMap)>) -> ScopesMap {
  let mut entries: Vec<(String, ModuleSpecifierMap)> = entries
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
  entries.sort_by(|(a, _), (b, _)| super::types::code_unit_cmp(b, a));
  ScopesMap { entries }
}

#[test]
fn resolves_most_specific_prefix_match() {
  let import_map = ImportMap {
    imports: module_specifier_map(vec![
      ("foo/", Some("https://cdn.example/foo/")),
      ("foo/bar/", Some("https://cdn.example/foo/bar/")),
    ]),
    scopes: ScopesMap::default(),
    integrity: ModuleIntegrityMap::default(),
  };

  let mut state = ImportMapState {
    import_map,
    ..ImportMapState::default()
  };
  let base_url = url("https://example.test/app/main.js");
  let resolved = resolve_module_specifier(&mut state, "foo/bar/baz.js", &base_url).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/foo/bar/baz.js");
}

#[test]
fn resolves_scoped_over_unscoped_and_falls_back_to_less_specific_scope() {
  let import_map = ImportMap {
    imports: module_specifier_map(vec![("foo/", Some("https://cdn.example/global/foo/"))]),
    scopes: scopes_map(vec![
      (
        "https://example.test/app/sub/",
        ModuleSpecifierMap::default(),
      ),
      (
        "https://example.test/app/",
        module_specifier_map(vec![("foo/", Some("https://cdn.example/app/foo/"))]),
      ),
    ]),
    integrity: ModuleIntegrityMap::default(),
  };

  let mut state = ImportMapState {
    import_map,
    ..ImportMapState::default()
  };
  let base_url = url("https://example.test/app/sub/main.js");
  let resolved = resolve_module_specifier(&mut state, "foo/a.js", &base_url).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/app/foo/a.js");

  let other_base = url("https://example.test/other/main.js");
  let resolved = resolve_module_specifier(&mut state, "foo/a.js", &other_base).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/global/foo/a.js");
}

#[test]
fn resolves_url_like_special_specifier_with_prefix_map() {
  let import_map = ImportMap {
    imports: module_specifier_map(vec![(
      "https://example.test/pkg/",
      Some("https://cdn.example/pkg/"),
    )]),
    scopes: ScopesMap::default(),
    integrity: ModuleIntegrityMap::default(),
  };

  let mut state = ImportMapState {
    import_map,
    ..ImportMapState::default()
  };
  let base_url = url("https://example.test/app/main.js");
  let resolved =
    resolve_module_specifier(&mut state, "https://example.test/pkg/mod.js", &base_url).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/pkg/mod.js");
}

#[test]
fn does_not_apply_prefix_matches_to_url_like_non_special_specifiers() {
  let import_map = ImportMap {
    imports: module_specifier_map(vec![
      ("mailto:foo/", Some("https://cdn.example/mailto/")),
      ("mailto:foo/bar", Some("https://cdn.example/exact")),
    ]),
    scopes: ScopesMap::default(),
    integrity: ModuleIntegrityMap::default(),
  };

  let mut state = ImportMapState {
    import_map,
    ..ImportMapState::default()
  };
  let base_url = url("https://example.test/app/main.js");
  let resolved = resolve_module_specifier(&mut state, "mailto:foo/bar", &base_url).unwrap();
  assert_eq!(resolved.as_str(), "https://cdn.example/exact");

  let resolved = resolve_module_specifier(&mut state, "mailto:foo/baz", &base_url).unwrap();
  assert_eq!(resolved.as_str(), "mailto:foo/baz");
}

#[test]
fn throws_on_bare_specifier_without_mapping() {
  let import_map = ImportMap::default();
  let mut state = ImportMapState {
    import_map,
    ..ImportMapState::default()
  };
  let base_url = url("https://example.test/app/main.js");
  let err = resolve_module_specifier(&mut state, "bare", &base_url).unwrap_err();
  assert!(matches!(err, ImportMapError::TypeError(_)));
}

#[test]
fn blocks_backtracking_above_prefix_mapping() {
  let import_map = ImportMap {
    imports: module_specifier_map(vec![("pkg/", Some("https://example.test/base/"))]),
    scopes: ScopesMap::default(),
    integrity: ModuleIntegrityMap::default(),
  };

  let mut state = ImportMapState {
    import_map,
    ..ImportMapState::default()
  };
  let base_url = url("https://example.test/app/main.js");
  let err = resolve_module_specifier(&mut state, "pkg/../evil.js", &base_url).unwrap_err();
  assert!(matches!(err, ImportMapError::TypeError(_)));
}
