use crate::style::custom_property_store::CustomPropertyStore;
use crate::style::values::CustomPropertyValue;
use crate::style::var_resolution::resolve_var_for_property;
use crate::PropertyValue;
use std::panic::AssertUnwindSafe;

fn make_props(pairs: &[(&str, &str)]) -> CustomPropertyStore {
  let mut store = CustomPropertyStore::default();
  for (name, value) in pairs.iter().copied() {
    store.insert(name.into(), CustomPropertyValue::new(value, None));
  }
  store
}

#[test]
fn var_resolution_token_splicing_never_panics_on_gnarly_token_streams() {
  let props = make_props(&[("--n", "0")]);

  let cases: &[(&str, Option<&str>)] = &[
    // Fast substring-splicing path (no tokenizer).
    ("var(--n)calc(1px)", Some("0 calc(1px)")),
    // Tokenizer path with nested blocks + comments + quoted string containing both quote types.
    (
      r#"if(false: var(--missing)calc(1px)/*comment*/([{"a'b\"c"}]); var(--n)calc(1px) 'data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg"/>' )"#,
      Some(r#"0 calc(1px) 'data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg"/>'"#),
    ),
    // Unbalanced input should be rejected as invalid syntax but never panic.
    ("var(--n)calc(1px", None),
  ];

  for &(raw, expected) in cases {
    let value = PropertyValue::Keyword(raw.to_string());
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
      resolve_var_for_property(&value, &props, "")
    }));
    assert!(
      result.is_ok(),
      "resolve_var_for_property panicked for `{raw}`"
    );

    if let Some(expected) = expected {
      let resolved = result.unwrap();
      assert_eq!(resolved.css_text(), Some(expected));
    }
  }
}
