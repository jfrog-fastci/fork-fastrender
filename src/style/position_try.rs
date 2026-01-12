use crate::css::types::Declaration;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry of `@position-try` rules available within a given tree scope.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PositionTryRegistry {
  rules: HashMap<String, Arc<[Declaration]>>,
}

impl PositionTryRegistry {
  fn is_accepted_property(property: &str) -> bool {
    // CSS Anchor Positioning: https://drafts.csswg.org/css-anchor-position-1/#fallback-rule
    //
    // `@position-try` rules are intentionally restricted to properties that affect only the size
    // and position of the box itself. Properties qualified with `!important` are invalid and
    // ignored (similar to `@keyframes` rules).
    matches!(
      property,
      // Inset properties.
      "top"
        | "right"
        | "bottom"
        | "left"
        | "inset"
        | "inset-inline"
        | "inset-inline-start"
        | "inset-inline-end"
        | "inset-block"
        | "inset-block-start"
        | "inset-block-end"
        // Margin properties.
        | "margin"
        | "margin-top"
        | "margin-right"
        | "margin-bottom"
        | "margin-left"
        | "margin-inline"
        | "margin-inline-start"
        | "margin-inline-end"
        | "margin-block"
        | "margin-block-start"
        | "margin-block-end"
        // Sizing properties.
        | "width"
        | "height"
        | "min-width"
        | "min-height"
        | "max-width"
        | "max-height"
        | "inline-size"
        | "block-size"
        | "min-inline-size"
        | "min-block-size"
        | "max-inline-size"
        | "max-block-size"
        | "aspect-ratio"
        | "box-sizing"
        // Self-alignment properties.
        | "align-self"
        | "justify-self"
        | "place-self"
        // Anchor positioning additions.
        | "position-anchor"
        | "position-area"
    )
  }

  pub fn get(&self, name: &str) -> Option<&[Declaration]> {
    self.rules.get(name).map(|decls| decls.as_ref())
  }

  pub fn register(&mut self, name: String, declarations: Vec<Declaration>) {
    let filtered: Vec<Declaration> = declarations
      .into_iter()
      .filter(|decl| {
        !decl.important
          && !decl.property.is_custom()
          && Self::is_accepted_property(decl.property.as_str())
      })
      .collect();
    self.rules.insert(name, Arc::from(filtered));
  }
}
