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
  let mut db = FontDatabase::empty();
  db.load_font_data(DEJAVU_SANS_FONT.to_vec())
    .expect("fixture font should load");
  let family = db
    .first_font()
    .expect("fixture font should be present")
    .family
    .clone();
  let font_context = FontContext::with_database(Arc::new(db));

  let dom = dom::parse_html(r#"<div id="t">A</div>"#).expect("parse html");
  let css = format!(
    r#"
      @font-feature-values "{family}" {{ @styleset {{ disambiguation: 2; }} }}
      #t {{
        font-family: "{family}";
        font-size: 16px;
        font-variant-alternates: styleset(disambiguation);
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
}
