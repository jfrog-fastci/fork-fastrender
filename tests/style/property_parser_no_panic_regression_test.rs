use fastrender::css::parser::parse_stylesheet;
use fastrender::css::properties::parse_property_value;
use fastrender::css::types::PropertyValue;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media_target_and_imports;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn property_parser_does_not_panic_on_hostile_inputs() {
  #[derive(Clone, Copy)]
  enum Expectation {
    Some,
    None,
    Either,
  }

  // Regression coverage for former panic-prone code paths in `css::properties`:
  // - `rotate: x 90deg` exercises axis/angle parsing (`axis.unwrap()`/`angle.unwrap()`).
  // - `url(\41)` exercises hex escape decoding (`to_digit(16).unwrap()`).
  // - `radial-gradient(at left top, ...)` exercises multi-part "at <position>" parsing
  //   (`parts_buf[n].unwrap()`).
  let cases: &[(&str, &str, Expectation)] = &[
    ("rotate", "x 90deg", Expectation::Some),
    ("rotate", "90deg x", Expectation::Some),
    ("rotate", "90deg x y", Expectation::None),
    ("translate", "10px 20% 30%", Expectation::None),
    ("scale", "calc(95%)", Expectation::Some),
    ("transform", "translateX(-100%) translateY(50%)", Expectation::Some),
    ("background-image", r"url(\41)", Expectation::Some),
    ("background-image", r"url(\))", Expectation::Some),
    (
      "background-image",
      "radial-gradient(at left top, red, blue)",
      Expectation::Some,
    ),
    (
      "background-image",
      "radial-gradient(at left top, red)",
      // This is syntactically invalid per CSS Images (needs >= 2 stops), but the property-value
      // parser may preserve it as a raw Keyword for downstream validation.
      Expectation::Either,
    ),
    ("color", "#ggg", Expectation::None),
    // Shorthands and multi-value tokenization.
    (
      "background",
      r"url(\41) center/cover no-repeat, linear-gradient(red, blue)",
      Expectation::Some,
    ),
    // Intentionally malformed nested function arguments (should be rejected, not panic).
    ("transform", "translate(10px,)", Expectation::Either),
  ];

  for (property, value, expectation) in cases {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      parse_property_value(property, value)
    }));

    assert!(
      result.is_ok(),
      "parse_property_value panicked for `{property}: {value}`"
    );

    let parsed = result.unwrap();
    match expectation {
      Expectation::Some => assert!(
        parsed.is_some(),
        "expected `{property}: {value}` to parse successfully"
      ),
      Expectation::None => assert!(parsed.is_none(), "expected `{property}: {value}` to be rejected"),
      Expectation::Either => {}
    }
  }

  // Validate a couple of "should parse" results so we don't accidentally regress outcomes for
  // valid inputs while making parsing total.
  let Some(PropertyValue::Url(url)) = parse_property_value("background-image", r"url(\41)") else {
    panic!("expected url(\\41) to parse as Url(...)");
  };
  assert_eq!(url, "A");

  let Some(PropertyValue::Url(url)) = parse_property_value("background-image", r"url(\))") else {
    panic!("expected url(\\)) to parse as Url(...)");
  };
  assert_eq!(url, ")");

  assert!(matches!(
    parse_property_value("background-image", "radial-gradient(at left top, red, blue)"),
    Some(PropertyValue::RadialGradient { .. })
  ));
}

#[test]
fn stylesheet_with_many_malformed_declarations_does_not_panic_in_cascade() {
  let html = r#"<div id="t">text</div>"#;
  let css = r#"
    .never-match {
      /* Malformed / edge-case declarations; should be ignored without panicking. */
      rotate: x;
      rotate: x 90deg;
      translate: 10px 20% 30%;
      scale: calc(95%);
      transform: translate(10px,);
      background-image: radial-gradient(at left top, red);
      background-image: radial-gradient(at left top, red, blue);
      background-image: url(\41);
      background-image: url(\));
      color: #ggg;
      margin: 10px foo;
      border-radius: 10px /;
    }

    #t { opacity: 0.5; }
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media_target_and_imports(
    &dom, &stylesheet, &media, None, None, None, None, None, None,
  );

  let node = find_by_id(&styled, "t").expect("node");
  assert!((node.styles.opacity - 0.5).abs() < 1e-6);
}

