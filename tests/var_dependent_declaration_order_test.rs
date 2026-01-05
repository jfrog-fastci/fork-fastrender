use fastrender::css::types::PropertyValue;
use fastrender::geometry::Size;
use fastrender::style::values::CustomPropertyValue;
use fastrender::style::{ComputedStyle, VarDependentDeclaration};
use fastrender::Rgba;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn recompute_var_dependent_properties_preserves_cascade_order() {
  let parent_styles = ComputedStyle::default();
  let viewport = Size::new(100.0, 100.0);

  // `ComputedStyle::recompute_var_dependent_properties` used to iterate over a `HashMap`, making the
  // reapply order non-deterministic. Some computed values depend on the already-applied state
  // (e.g. `border-*-color: currentcolor` depends on `color`), so ensure we always apply in the
  // original cascade order.
  for _ in 0..64 {
    let mut styles = ComputedStyle::default();
    styles.color = Rgba::new(0, 255, 0, 1.0);
    styles.border_bottom_color = Rgba::new(0, 255, 0, 1.0);

    // Simulate `color` being var()-dependent and updated by a custom property change.
    styles
      .custom_properties
      .insert(Arc::from("--text-color"), CustomPropertyValue::new("#ff0000", None));

    // Make the border color depend on `currentcolor`, but via `var()` so it participates in the
    // var-dependent recomputation path.
    styles.custom_properties.insert(
      Arc::from("--border-color"),
      CustomPropertyValue::new("currentcolor", None),
    );

    let mut var_deps: HashMap<&'static str, VarDependentDeclaration> =
      HashMap::with_hasher(RandomState::new());
    var_deps.insert(
      "border-bottom-color",
      VarDependentDeclaration {
        order: 1,
        value: PropertyValue::Custom("var(--border-color)".into()),
      },
    );
    var_deps.insert(
      "color",
      VarDependentDeclaration {
        order: 0,
        value: PropertyValue::Custom("var(--text-color)".into()),
      },
    );
    styles.var_dependent_declarations = Arc::new(var_deps);

    styles.recompute_var_dependent_properties(&parent_styles, viewport);

    assert_eq!(styles.color, Rgba::new(255, 0, 0, 1.0));
    assert_eq!(styles.border_bottom_color, Rgba::new(255, 0, 0, 1.0));
  }
}

