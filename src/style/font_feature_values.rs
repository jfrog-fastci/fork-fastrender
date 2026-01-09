use crate::css::types::{FontFeatureValuesGroup, FontFeatureValuesRule};
use rustc_hash::FxHashMap;

/// Registry of `@font-feature-values` rules available to styles.
///
/// The registry is constructed in cascade order (later rules override earlier ones, including
/// cascade layers) and supports lookup by `(family, group, name)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontFeatureValuesRegistry {
  /// family (ASCII lowercase) -> group -> name (ASCII lowercase) -> values
  values: FxHashMap<String, FxHashMap<FontFeatureValuesGroup, FxHashMap<String, Vec<u8>>>>,
}

impl FontFeatureValuesRegistry {
  /// Register a parsed `@font-feature-values` rule using cascade order (later overrides earlier).
  pub fn register(&mut self, rule: FontFeatureValuesRule) {
    let FontFeatureValuesRule {
      font_families,
      groups,
    } = rule;

    if font_families.is_empty() || groups.is_empty() {
      return;
    }

    for family in font_families {
      let family_key = family.to_ascii_lowercase();
      let family_entry = self.values.entry(family_key).or_default();

      for (group, feature_map) in &groups {
        let group_entry = family_entry.entry(*group).or_default();
        for (name, values) in feature_map {
          if values.is_empty() {
            continue;
          }
          group_entry.insert(name.to_ascii_lowercase(), values.clone());
        }
      }
    }
  }

  /// Lookup a named feature value list for a particular font family and group.
  pub fn lookup(
    &self,
    family_name: &str,
    group: FontFeatureValuesGroup,
    feature_value_name: &str,
  ) -> Option<&[u8]> {
    let family_key = family_name.to_ascii_lowercase();
    let group_map = self.values.get(&family_key)?.get(&group)?;
    let name_key = feature_value_name.to_ascii_lowercase();
    group_map.get(&name_key).map(|values| values.as_slice())
  }
}
