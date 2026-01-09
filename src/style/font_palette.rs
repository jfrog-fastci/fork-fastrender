use crate::css::types::{FontPaletteBase, FontPaletteOverride, FontPaletteValuesRule};
use crate::style::color::{Color, Rgba};
use crate::style::types::FontPalette;
use std::collections::HashMap;

/// Registry of @font-palette-values rules available to styles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontPaletteRegistry {
  rules: HashMap<String, Vec<FontPaletteValuesRule>>,
}

impl FontPaletteRegistry {
  /// Register a parsed @font-palette-values rule using cascade order (later rules override earlier ones).
  pub fn register(&mut self, rule: FontPaletteValuesRule) {
    let key = rule.name.to_ascii_lowercase();
    self.rules.entry(key).or_default().push(rule);
  }

  /// Resolve a named palette for a specific font family, returning the last matching rule.
  fn rule_for(&self, name: &str, font_family: &str) -> Option<&FontPaletteValuesRule> {
    let key = name.to_ascii_lowercase();
    let candidates = self.rules.get(&key)?;
    candidates.iter().rev().find(|rule| {
      rule
        .font_families
        .iter()
        .any(|f| f.eq_ignore_ascii_case(font_family))
    })
  }
}

/// Fully resolved palette selection for a text run.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedFontPalette {
  pub base: FontPaletteBase,
  pub overrides: Vec<(u16, Rgba)>,
  pub override_hash: u64,
}

fn hash_overrides(overrides: &[(u16, Rgba)]) -> u64 {
  use rustc_hash::FxHasher;
  use std::hash::Hash;
  use std::hash::Hasher;

  let mut hasher = FxHasher::default();
  for (idx, color) in overrides {
    idx.hash(&mut hasher);
    color.r.hash(&mut hasher);
    color.g.hash(&mut hasher);
    color.b.hash(&mut hasher);
    color.alpha_u8().hash(&mut hasher);
  }
  hasher.finish()
}

/// Resolve the palette choice for a run given the authored font-palette value, registry, and font.
pub fn resolve_font_palette_for_font(
  palette: &FontPalette,
  registry: &FontPaletteRegistry,
  font_family: &str,
  current_color: Rgba,
  is_dark_color_scheme: bool,
  forced_colors: bool,
) -> ResolvedFontPalette {
  let (base, overrides): (FontPaletteBase, Vec<FontPaletteOverride>) = match palette {
    FontPalette::Normal => (FontPaletteBase::Normal, Vec::new()),
    FontPalette::Light => (FontPaletteBase::Light, Vec::new()),
    FontPalette::Dark => (FontPaletteBase::Dark, Vec::new()),
    FontPalette::Named(name) => registry
      .rule_for(name, font_family)
      .map(|rule| (rule.base_palette, rule.overrides.clone()))
      .unwrap_or((FontPaletteBase::Normal, Vec::new())),
  };

  let resolved_overrides: Vec<(u16, Rgba)> = overrides
    .into_iter()
    .map(|ov| {
      let resolved = match ov.color {
        Color::CurrentColor => Rgba {
          a: 1.0,
          ..current_color
        },
        _ => ov
          .color
          .to_rgba_with_scheme_and_forced_colors(current_color, is_dark_color_scheme, forced_colors),
      };
      (ov.index, resolved)
    })
    .collect();

  let override_hash = if resolved_overrides.is_empty() {
    0
  } else {
    hash_overrides(&resolved_overrides)
  };

  ResolvedFontPalette {
    base,
    overrides: resolved_overrides,
    override_hash,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolves_light_dark_overrides_using_color_scheme() {
    let mut registry = FontPaletteRegistry::default();
    let mut rule = FontPaletteValuesRule::new("custom");
    rule.font_families.push("Example".to_string());
    rule.base_palette = FontPaletteBase::Normal;
    rule.overrides.push(FontPaletteOverride {
      index: 1,
      color: Color::LightDark {
        light: Box::new(Color::Rgba(Rgba::RED)),
        dark: Box::new(Color::Rgba(Rgba::BLUE)),
      },
    });
    registry.register(rule);

    let palette = FontPalette::Named("custom".to_string());
    let light =
      resolve_font_palette_for_font(&palette, &registry, "Example", Rgba::BLACK, false, false);
    assert_eq!(light.overrides, vec![(1, Rgba::RED)]);

    let dark =
      resolve_font_palette_for_font(&palette, &registry, "Example", Rgba::BLACK, true, false);
    assert_eq!(dark.overrides, vec![(1, Rgba::BLUE)]);
  }
}
