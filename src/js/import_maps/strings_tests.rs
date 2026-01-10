use super::merge_existing_and_new_import_maps;
use super::parse_import_map_string;
use super::strings::cmp_code_units;
use super::types::{is_code_unit_prefix, ImportMap, ImportMapState, ModuleIntegrityMap, ModuleSpecifierMap, ScopesMap};
use std::cmp::Ordering;
use url::Url;

#[test]
fn cmp_code_units_orders_by_utf16_code_units() {
  assert_eq!(cmp_code_units("😀", "\u{E000}"), Ordering::Less);
  assert_eq!("😀".cmp("\u{E000}"), Ordering::Greater);
}

#[test]
fn is_code_unit_prefix_handles_non_ascii() {
  assert!(is_code_unit_prefix("😀", "😀/x"));
  assert!(is_code_unit_prefix("😀/", "😀/x"));
  assert!(!is_code_unit_prefix("😀/y", "😀/x"));
}

#[test]
fn parse_sorts_module_specifier_map_descending_by_utf16_code_units() {
  let base_url = Url::parse("https://example.com/").unwrap();
  let (map, warnings) = parse_import_map_string(
    r#"{ "imports": { "\uD83D\uDE00": "/a.js", "\uE000": "/b.js" } }"#,
    &base_url,
  )
  .unwrap();
  assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

  let keys: Vec<&str> = map.imports.entries.iter().map(|(k, _)| k.as_str()).collect();
  assert_eq!(keys, vec!["\u{E000}", "😀"]);
}

#[test]
fn scopes_are_sorted_descending_by_utf16_code_units() {
  let mut state = ImportMapState::default();

  let new_import_map = ImportMap {
    imports: ModuleSpecifierMap::default(),
    scopes: ScopesMap {
      entries: vec![
        ("😀/".to_string(), ModuleSpecifierMap::default()),
        ("\u{E000}/".to_string(), ModuleSpecifierMap::default()),
      ],
    },
    integrity: ModuleIntegrityMap::default(),
  };

  merge_existing_and_new_import_maps(&mut state, &new_import_map);

  let keys: Vec<&str> = state
    .import_map
    .scopes
    .entries
    .iter()
    .map(|(k, _)| k.as_str())
    .collect();
  assert_eq!(keys, vec!["\u{E000}/", "😀/"]);
}
