use crate::css::types::{FontFeatureValueType, FontFeatureValuesRule};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FontFeatureValuesFamily {
  stylistic: HashMap<String, Vec<u32>>,
  historical_forms: HashMap<String, Vec<u32>>,
  styleset: HashMap<String, Vec<u32>>,
  character_variant: HashMap<String, Vec<u32>>,
  swash: HashMap<String, Vec<u32>>,
  ornaments: HashMap<String, Vec<u32>>,
  annotation: HashMap<String, Vec<u32>>,
}

impl FontFeatureValuesFamily {
  fn map_mut(&mut self, ty: FontFeatureValueType) -> &mut HashMap<String, Vec<u32>> {
    match ty {
      FontFeatureValueType::Stylistic => &mut self.stylistic,
      FontFeatureValueType::HistoricalForms => &mut self.historical_forms,
      FontFeatureValueType::Styleset => &mut self.styleset,
      FontFeatureValueType::CharacterVariant => &mut self.character_variant,
      FontFeatureValueType::Swash => &mut self.swash,
      FontFeatureValueType::Ornaments => &mut self.ornaments,
      FontFeatureValueType::Annotation => &mut self.annotation,
    }
  }

  fn map(&self, ty: FontFeatureValueType) -> &HashMap<String, Vec<u32>> {
    match ty {
      FontFeatureValueType::Stylistic => &self.stylistic,
      FontFeatureValueType::HistoricalForms => &self.historical_forms,
      FontFeatureValueType::Styleset => &self.styleset,
      FontFeatureValueType::CharacterVariant => &self.character_variant,
      FontFeatureValueType::Swash => &self.swash,
      FontFeatureValueType::Ornaments => &self.ornaments,
      FontFeatureValueType::Annotation => &self.annotation,
    }
  }
}

/// Registry of `@font-feature-values` rules available to styles.
///
/// The registry is constructed in cascade order (later rules override earlier ones, including
/// cascade layers) and supports lookup by `(family, feature-type, name)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontFeatureValuesRegistry {
  /// Font family name (ASCII lowercase) -> per-family feature-value maps.
  families: HashMap<String, FontFeatureValuesFamily>,
}

impl FontFeatureValuesRegistry {
  /// Register a parsed `@font-feature-values` rule using cascade order (later overrides earlier).
  ///
  /// Each feature-value declaration is applied to every family listed in the rule prelude.
  pub fn register(&mut self, rule: FontFeatureValuesRule) {
    let FontFeatureValuesRule {
      font_families,
      groups,
    } = rule;

    if font_families.is_empty() || groups.is_empty() {
      return;
    }

    for family in font_families {
      let key = family.to_ascii_lowercase();
      let family_entry = self.families.entry(key).or_default();

      for (ty, feature_map) in &groups {
        let ty_entry = family_entry.map_mut(*ty);
        for (name, values) in feature_map {
          if values.is_empty() {
            continue;
          }
          // Feature value names are case-sensitive.
          ty_entry.insert(name.clone(), values.clone());
        }
      }
    }
  }

  /// Lookup a named feature value list for a particular font family and type.
  ///
  /// Font family names are matched case-insensitively (ASCII); feature value names are
  /// case-sensitive.
  pub fn lookup(&self, family: &str, ty: FontFeatureValueType, name: &str) -> Option<&[u32]> {
    let key = family.to_ascii_lowercase();
    let family = self.families.get(&key)?;
    family.map(ty).get(name).map(|values| values.as_slice())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rustc_hash::FxHashMap;

  #[test]
  fn last_tuple_wins() {
    let mut registry = FontFeatureValuesRegistry::default();

    let mut first = FontFeatureValuesRule::new(vec!["Example".to_string()]);
    first.groups.insert(
      FontFeatureValueType::Styleset,
      FxHashMap::from_iter([("alt".to_string(), vec![1u32])]),
    );
    registry.register(first);

    let mut second = FontFeatureValuesRule::new(vec!["Example".to_string()]);
    second.groups.insert(
      FontFeatureValueType::Styleset,
      FxHashMap::from_iter([("alt".to_string(), vec![2u32])]),
    );
    registry.register(second);

    assert_eq!(
      registry.lookup("example", FontFeatureValueType::Styleset, "alt"),
      Some([2u32].as_slice())
    );
  }

  #[test]
  fn multiple_families_prelude_applies_to_each_family() {
    let mut registry = FontFeatureValuesRegistry::default();

    let mut rule = FontFeatureValuesRule::new(vec!["A".to_string(), "B".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::Swash,
      FxHashMap::from_iter([("swashy".to_string(), vec![7u32])]),
    );
    registry.register(rule);

    assert_eq!(
      registry.lookup("a", FontFeatureValueType::Swash, "swashy"),
      Some([7u32].as_slice())
    );
    assert_eq!(
      registry.lookup("B", FontFeatureValueType::Swash, "swashy"),
      Some([7u32].as_slice())
    );
  }

  #[test]
  fn respects_cascade_layer_order() {
    use crate::css::parser::parse_stylesheet;
    use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
    use crate::style::cascade::apply_styles;

    let css = r#"
      @layer a, b;

      @layer a {
        @font-feature-values "Inter" { @styleset { disambiguation: 1; } }
      }

      @layer b {
        @font-feature-values "Inter" { @styleset { disambiguation: 2; } }
      }
    "#;
    let sheet = parse_stylesheet(css).expect("stylesheet should parse");
    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };
    let styled = apply_styles(&dom, &sheet);

    assert_eq!(
      styled.styles.font_feature_values.lookup(
        "Inter",
        FontFeatureValueType::Styleset,
        "disambiguation"
      ),
      Some([2u32].as_slice())
    );
  }
}
