use super::{merge_module_specifier_maps, ModuleSpecifierMap};

use url::Url;

fn map(entries: Vec<(&str, Option<&str>)>) -> ModuleSpecifierMap {
  ModuleSpecifierMap {
    entries: entries
      .into_iter()
      .map(|(k, v)| (k.to_string(), v.map(|u| Url::parse(u).unwrap())))
      .collect(),
  }
}

#[test]
fn merge_preserves_existing_keys_on_conflict() {
  let old = map(vec![("a", Some("https://example.com/a.js"))]);
  let new = map(vec![("a", Some("https://example.com/other.js"))]);

  let merged = merge_module_specifier_maps(&new, &old);
  assert_eq!(merged.entries.len(), 1);
  assert_eq!(merged.entries[0].0, "a");
  assert_eq!(
    merged.entries[0].1.as_ref().unwrap().as_str(),
    "https://example.com/a.js"
  );
}

#[test]
fn merge_keeps_descending_code_unit_sorting() {
  // Note: keys are expected to be in descending UTF-16 code unit order to preserve the spec's
  // "first match wins" semantics.
  let old = map(vec![("a", Some("https://example.com/a.js"))]);
  let new = map(vec![("b", Some("https://example.com/b.js"))]);

  let merged = merge_module_specifier_maps(&new, &old);
  let keys: Vec<_> = merged.entries.iter().map(|(k, _)| k.as_str()).collect();
  assert_eq!(keys, vec!["b", "a"]);
}
