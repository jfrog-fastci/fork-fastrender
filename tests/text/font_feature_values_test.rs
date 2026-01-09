use std::collections::HashMap;
use std::sync::Arc;

use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::{CssRule, FontFeatureValueType};
use fastrender::dom;
use fastrender::style::cascade::{apply_styles, StyledNode};
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::FontContext;
use fastrender::text::pipeline::{assign_fonts, Direction, ItemizedRun, Script};

const DEJAVU_SANS_FONT: &[u8] = include_bytes!("../fixtures/fonts/DejaVuSans-subset.ttf");

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn load_fixture_font_context() -> (FontContext, String) {
  let mut db = FontDatabase::empty();
  db.load_font_data(DEJAVU_SANS_FONT.to_vec())
    .expect("fixture font should load");
  let family = db
    .first_font()
    .expect("fixture font should be present")
    .family
    .clone();
  (FontContext::with_database(Arc::new(db)), family)
}

#[test]
fn parses_font_feature_values_rules() {
  let css = r#"
    @font-feature-values "Inter", Gentium Book {
      @styleset { disambiguation: 2; alt: 1 2; }
      @swash { swashy: 7; }
    }
  "#;
  let sheet = parse_stylesheet(css).expect("stylesheet should parse");
  assert_eq!(sheet.rules.len(), 1);

  let rule = match &sheet.rules[0] {
    CssRule::FontFeatureValues(rule) => rule,
    other => panic!("unexpected rule parsed: {other:?}"),
  };

  assert_eq!(
    rule.font_families,
    vec!["Inter".to_string(), "Gentium Book".to_string()]
  );

  let styleset = rule
    .groups
    .get(&FontFeatureValueType::Styleset)
    .expect("expected @styleset group");
  assert_eq!(styleset.get("disambiguation"), Some(&vec![2u32]));
  assert_eq!(styleset.get("alt"), Some(&vec![1u32, 2u32]));

  let swash = rule
    .groups
    .get(&FontFeatureValueType::Swash)
    .expect("expected @swash group");
  assert_eq!(swash.get("swashy"), Some(&vec![7u32]));
}

#[test]
fn font_feature_values_registry_respects_layer_order() {
  let dom = dom::parse_html(r#"<div id="t"></div>"#).expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      @layer a, b;

      @layer a {
        @font-feature-values "Inter" { @styleset { disambiguation: 1; } }
      }

      @layer b {
        @font-feature-values "Inter" { @styleset { disambiguation: 2; } }
      }
    "#,
  )
  .expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  assert_eq!(
    node.styles.font_feature_values.lookup(
      "Inter",
      FontFeatureValueType::Styleset,
      "disambiguation"
    ),
    Some([2u32].as_slice())
  );
}

#[test]
fn font_variant_alternates_named_values_resolve_via_font_feature_values() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{
        @stylistic {{ Fancy: 3; }}
        @styleset {{ disambiguation: 2; }}
        @character-variant {{ Var: 4; }}
        @swash {{ Swishy: 7; }}
        @ornaments {{ Flourish: 2; }}
        @annotation {{ Note: 1; }}
      }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: stylistic(Fancy) styleset(disambiguation) character-variant(Var)
          swash(Swishy) ornaments(Flourish) annotation(Note);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  // Sanity-check the registry exists on computed style and can be queried.
  assert_eq!(
    node.styles.font_feature_values.lookup(
      &family,
      FontFeatureValueType::Styleset,
      "disambiguation"
    ),
    Some([2u32].as_slice())
  );
  assert_eq!(
    node.styles
      .font_feature_values
      .lookup(&family, FontFeatureValueType::Ornaments, "Flourish"),
    Some([2u32].as_slice())
  );

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert_eq!(
    seen.get(b"ss02"),
    Some(&1),
    "styleset(disambiguation) should enable ss02 via @font-feature-values"
  );
  assert_eq!(
    seen.get(b"salt"),
    Some(&3),
    "stylistic(Fancy) should map to OpenType salt 3 via @font-feature-values"
  );
  assert_eq!(
    seen.get(b"cv04"),
    Some(&1),
    "character-variant(Var) should map to OpenType cv04 via @font-feature-values"
  );
  assert_eq!(
    seen.get(b"swsh"),
    Some(&7),
    "swash(Swishy) should map to OpenType swsh 7 via @font-feature-values"
  );
  assert_eq!(
    seen.get(b"cswh"),
    Some(&7),
    "swash(Swishy) should map to OpenType cswh 7 via @font-feature-values"
  );
  assert_eq!(
    seen.get(b"ornm"),
    Some(&2),
    "ornaments(Flourish) should map to OpenType ornm 2 via @font-feature-values"
  );
  assert_eq!(
    seen.get(b"nalt"),
    Some(&1),
    "annotation(Note) should map to OpenType nalt 1 via @font-feature-values"
  );
}

#[test]
fn font_feature_values_merges_repeated_blocks() {
  let css = r#"
    @font-feature-values Foo {
      @styleset { a: 1; }
      @styleset { b: 2; }
    }
  "#;
  let sheet = parse_stylesheet(css).expect("stylesheet should parse");
  assert_eq!(sheet.rules.len(), 1);

  let rule = match &sheet.rules[0] {
    CssRule::FontFeatureValues(rule) => rule,
    other => panic!("unexpected rule parsed: {other:?}"),
  };

  let styleset = rule
    .groups
    .get(&FontFeatureValueType::Styleset)
    .expect("expected @styleset group");
  assert_eq!(styleset.get("a"), Some(&vec![1u32]));
  assert_eq!(styleset.get("b"), Some(&vec![2u32]));
}

#[test]
fn font_feature_values_keeps_mixed_integer_values() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @styleset {{ mixed: 1 125; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: styleset(mixed);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert_eq!(
    seen.get(b"ss01"),
    Some(&1),
    "styleset(mixed) should enable ss01 even when additional indices are ignored"
  );
}

#[test]
fn font_variant_alternates_character_variant_two_integer_mapping_uses_second_as_value() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @character-variant {{ alpha-2: 1 2; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: character-variant(alpha-2);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert_eq!(
    seen.get(b"cv01"),
    Some(&2),
    "alpha-2: 1 2 should map to cv01 with value 2"
  );
  assert!(
    seen.get(b"cv02").is_none(),
    "alpha-2: 1 2 should not map to cv02"
  );
}

#[test]
fn font_variant_alternates_character_variant_last_wins() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @character-variant {{ zeta: 20 3; zeta-2: 20 2; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: character-variant(zeta, zeta-2);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert_eq!(
    seen.get(b"cv20"),
    Some(&2),
    "later character-variant() entries should override earlier ones for the same feature index"
  );
}

#[test]
fn font_variant_alternates_character_variant_invalid_three_integer_mapping_is_ignored() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @character-variant {{ bad: 5 3 6; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: character-variant(bad);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert!(
    seen.get(b"cv05").is_none(),
    "invalid @character-variant definitions with 3+ integers must be ignored"
  );
}

#[test]
fn font_variant_alternates_swash_named_multi_value_definition_is_ignored() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @swash {{ bad: 3 5; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: swash(bad);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert!(
    seen.get(b"swsh").is_none(),
    "@swash definitions with multiple integers must be ignored (no swsh feature)"
  );
  assert!(
    seen.get(b"cswh").is_none(),
    "@swash definitions with multiple integers must be ignored (no cswh feature)"
  );
}

#[test]
fn font_variant_alternates_annotation_named_multi_value_definition_is_ignored() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @annotation {{ bad: 1 2; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: annotation(bad);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert!(
    seen.get(b"nalt").is_none(),
    "@annotation definitions with multiple integers must be ignored (no nalt feature)"
  );
}

#[test]
fn font_variant_alternates_stylistic_named_multi_value_definition_is_ignored() {
  let (font_context, family) = load_fixture_font_context();

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @stylistic {{ bad: 2 3; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: stylistic(bad);
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let node = find_by_id(&styled, "t").expect("expected node");

  let text = "A";
  let run = ItemizedRun {
    text: text.to_string(),
    start: 0,
    end: text.len(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };
  let font_runs = assign_fonts(&[run], &node.styles, &font_context).expect("assign fonts");
  assert_eq!(font_runs.len(), 1, "expected a single font run");

  let mut seen: HashMap<[u8; 4], u32> = HashMap::new();
  for f in font_runs[0].features.iter() {
    seen.insert(f.tag.to_bytes(), f.value);
  }

  assert!(
    seen.get(b"salt").is_none(),
    "@stylistic definitions with multiple integers must be ignored (no salt feature)"
  );
}

#[test]
fn font_feature_values_rejects_generic_family_names() {
  let css = r#"@font-feature-values serif { @styleset { a: 1; } }"#;
  let sheet = parse_stylesheet(css).expect("stylesheet should parse");
  assert!(
    sheet
      .rules
      .iter()
      .all(|rule| !matches!(rule, CssRule::FontFeatureValues(_))),
    "generic family names should invalidate the entire @font-feature-values rule"
  );
}
