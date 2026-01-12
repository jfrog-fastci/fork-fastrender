use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::EastAsianVariant;
use fastrender::style::types::EastAsianWidth;
use fastrender::style::types::FontStretch;
use fastrender::style::types::FontStyle;
use fastrender::style::types::FontVariant;
use fastrender::style::types::FontVariantCaps;
use fastrender::style::types::FontVariantLigatures;
use fastrender::style::types::FontVariantPosition;
use fastrender::style::types::NumericFigure;
use fastrender::style::types::NumericFraction;
use fastrender::style::types::NumericSpacing;
use fastrender::style::types::{FontVariantAlternateValue, FontVariantAlternates, FontWeight};
use fastrender::style::types::{FontVariantEastAsian, FontVariantNumeric};

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
fn font_variant_keywords_are_ascii_case_insensitive() {
  let dom = dom::parse_html(
    r#"
      <div id="variant"></div>
      <div id="longhand"></div>
      <div id="font"></div>
      <div id="stretch"></div>
    "#,
  )
  .expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      #variant {
         font-variant: SMALL-CAPS OLDSTYLE-NUMS TABULAR-NUMS STACKED-FRACTIONS ORDINAL SLASHED-ZERO
           JIS90 PROPORTIONAL-WIDTH RUBY NO-COMMON-LIGATURES DISCRETIONARY-LIGATURES
          HISTORICAL-FORMS STYLESET(AltG,AltA) SWASH(Swishy) ANNOTATION(Note) SUB;
      }

      #longhand {
        font-variant-caps: ALL-SMALL-CAPS;
        font-variant-ligatures: NONE;
        font-variant-numeric: LINING-NUMS;
        font-variant-east-asian: JIS04 FULL-WIDTH;
        font-variant-position: SUPER;
        font-variant-alternates: STYLISTIC(Fancy) ANNOTATION(FooBar);
      }

      #font { font: ITALIC BOLD SMALL-CAPS CONDENSED 16px/20px serif; }
      #stretch { font-stretch: CONDENSED; }
    "#,
  )
  .expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let variant = find_by_id(&styled, "variant").expect("variant element");
  assert_eq!(variant.styles.font_variant, FontVariant::SmallCaps);
  assert_eq!(variant.styles.font_variant_caps, FontVariantCaps::SmallCaps);
  assert_eq!(
    variant.styles.font_variant_numeric,
    FontVariantNumeric {
      figure: NumericFigure::Oldstyle,
      spacing: NumericSpacing::Tabular,
      fraction: NumericFraction::Stacked,
      ordinal: true,
      slashed_zero: true,
    }
  );
  assert_eq!(
    variant.styles.font_variant_east_asian,
    FontVariantEastAsian {
      variant: Some(EastAsianVariant::Jis90),
      width: Some(EastAsianWidth::ProportionalWidth),
      ruby: true,
    }
  );
  assert_eq!(
    variant.styles.font_variant_ligatures,
    FontVariantLigatures {
      common: false,
      discretionary: true,
      historical: false,
      contextual: true,
    }
  );
  assert_eq!(
    variant.styles.font_variant_alternates,
    FontVariantAlternates {
      historical_forms: true,
      stylistic: None,
      stylesets: vec![
        FontVariantAlternateValue::Name("AltG".to_string()),
        FontVariantAlternateValue::Name("AltA".to_string()),
      ],
      character_variants: vec![],
      swash: Some(FontVariantAlternateValue::Name("Swishy".to_string())),
      ornaments: None,
      annotation: Some(FontVariantAlternateValue::Name("Note".to_string())),
    }
  );
  assert_eq!(
    variant.styles.font_variant_position,
    FontVariantPosition::Sub
  );

  let longhand = find_by_id(&styled, "longhand").expect("longhand element");
  assert_eq!(longhand.styles.font_variant, FontVariant::Normal);
  assert_eq!(
    longhand.styles.font_variant_caps,
    FontVariantCaps::AllSmallCaps
  );
  assert_eq!(
    longhand.styles.font_variant_ligatures,
    FontVariantLigatures {
      common: false,
      discretionary: false,
      historical: false,
      contextual: false,
    }
  );
  assert_eq!(
    longhand.styles.font_variant_numeric,
    FontVariantNumeric {
      figure: NumericFigure::Lining,
      ..FontVariantNumeric::default()
    }
  );
  assert_eq!(
    longhand.styles.font_variant_east_asian,
    FontVariantEastAsian {
      variant: Some(EastAsianVariant::Jis04),
      width: Some(EastAsianWidth::FullWidth),
      ruby: false,
    }
  );
  assert_eq!(
    longhand.styles.font_variant_position,
    FontVariantPosition::Super
  );
  assert_eq!(
    longhand.styles.font_variant_alternates,
    FontVariantAlternates {
      historical_forms: false,
      stylistic: Some(FontVariantAlternateValue::Name("Fancy".to_string())),
      stylesets: vec![],
      character_variants: vec![],
      swash: None,
      ornaments: None,
      annotation: Some(FontVariantAlternateValue::Name("FooBar".to_string())),
    }
  );

  let font = find_by_id(&styled, "font").expect("font element");
  assert_eq!(font.styles.font_style, FontStyle::Italic);
  assert_eq!(font.styles.font_weight, FontWeight::Bold);
  assert_eq!(font.styles.font_variant, FontVariant::SmallCaps);
  assert_eq!(font.styles.font_stretch, FontStretch::Condensed);

  let stretch = find_by_id(&styled, "stretch").expect("stretch element");
  assert_eq!(stretch.styles.font_stretch, FontStretch::Condensed);
}
