use crate::r#ref::compare::{compare_images, load_png_from_bytes, CompareConfig};
use fastrender::math::{
  layout_mathml, ColumnAlign, MathFragment, MathLengthOrKeyword, MathNode, MathVariant,
};
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::text::font_db::FontConfig;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::{FastRender, FragmentContent, FragmentNode};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;

fn find_math_fragment<'a>(fragment: &'a FragmentNode) -> Option<&'a ReplacedType> {
  match &fragment.content {
    FragmentContent::Replaced { replaced_type, .. } => {
      if matches!(replaced_type, ReplacedType::Math(_)) {
        Some(replaced_type)
      } else {
        None
      }
    }
    _ => fragment.children.iter().find_map(find_math_fragment),
  }
}

fn with_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
  thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(f)
    .expect("spawn math thread")
    .join()
    .expect("join math thread")
}

fn deterministic_renderer() -> FastRender {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer")
}

fn deterministic_renderer_with_fixture_fonts() -> FastRender {
  let font_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts");
  FastRender::builder()
    .font_sources(FontConfig::bundled_only().add_font_dir(font_dir))
    .build()
    .expect("renderer")
}

fn stix_math_font_context() -> FontContext {
  static CTX: OnceLock<FontContext> = OnceLock::new();
  CTX
    .get_or_init(|| {
      let font_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts");
      FontContext::with_config(FontConfig::bundled_only().add_font_dir(font_dir))
    })
    .clone()
}

fn stix_math_style(font_size: f32) -> fastrender::ComputedStyle {
  let mut style = fastrender::ComputedStyle::default();
  style.font_size = font_size;
  style.font_family = Arc::from(vec!["STIX Two Math".to_string()]);
  style
}

fn mo(text: &str) -> MathNode {
  MathNode::Operator {
    text: text.to_string(),
    form: None,
    stretchy: None,
    lspace: None,
    rspace: None,
    variant: None,
  }
}

#[test]
fn fraction_mathml_layouts_and_paints() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let dom = renderer
      .parse_html("<math><mfrac><mi>a</mi><mi>b</mi></mfrac></math>")
      .expect("dom");
    let fragments = renderer
      .layout_document(&dom, 200, 200)
      .expect("layout document");

    let replaced = find_math_fragment(&fragments.root).expect("math fragment");
    let ReplacedType::Math(math) = replaced else {
      panic!("expected math replaced type");
    };
    let layout = math.layout.as_ref().expect("math layout");
    assert!(layout.width > 0.0 && layout.height > 0.0);
    assert!(layout.baseline > 0.0 && layout.baseline < layout.height);

    let builder = DisplayListBuilder::new();
    let list = builder.build_tree(&fragments);
    assert!(
      list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::FillRect(_))),
      "fraction bar should emit a rule",
    );
    assert!(
      list
        .items()
        .iter()
        .any(|item| matches!(item, DisplayItem::Text(_))),
      "math text should emit glyphs",
    );
  });
}

#[test]
fn display_style_large_operator_is_larger_than_inline() {
  with_stack(|| {
    let style = stix_math_style(22.0);
    let font_ctx = stix_math_font_context();
    let inline = MathNode::Math {
      display_style: false,
      children: vec![mo("∑")],
    };
    let display = MathNode::Math {
      display_style: true,
      children: vec![mo("∑")],
    };
    let inline_layout = layout_mathml(&inline, &style, &font_ctx);
    let display_layout = layout_mathml(&display, &style, &font_ctx);
    assert!(
      display_layout.height > inline_layout.height + 0.5,
      "expected display-style ∑ to be taller than inline (inline={}, display={})",
      inline_layout.height,
      display_layout.height
    );
  });
}

#[test]
fn movable_limits_integral_moves_limits_to_scripts_in_inline() {
  with_stack(|| {
    let style = stix_math_style(22.0);
    let font_ctx = stix_math_font_context();

    let under = MathNode::Number {
      text: "12345678901234567890".into(),
      variant: None,
    };
    let over = MathNode::Number {
      text: "12345678901234567890".into(),
      variant: None,
    };
    let base = mo("∫");

    let inline = MathNode::Math {
      display_style: false,
      children: vec![MathNode::UnderOver {
        base: Box::new(base.clone()),
        under: Box::new(under.clone()),
        over: Box::new(over.clone()),
        accent: false,
        accentunder: false,
      }],
    };
    let display = MathNode::Math {
      display_style: true,
      children: vec![MathNode::UnderOver {
        base: Box::new(base),
        under: Box::new(under),
        over: Box::new(over),
        accent: false,
        accentunder: false,
      }],
    };

    let inline_layout = layout_mathml(&inline, &style, &font_ctx);
    let display_layout = layout_mathml(&display, &style, &font_ctx);

    // If movable limits are working, inline math should place limits as scripts (to the side),
    // making it wider, while display style keeps them stacked (less wide but taller).
    assert!(
      inline_layout.width > display_layout.width + 1.0,
      "expected inline ∫ limits to increase width (inline={}, display={})",
      inline_layout.width,
      display_layout.width
    );
    assert!(
      display_layout.height > inline_layout.height + 1.0,
      "expected display-style ∫ limits to increase height (inline={}, display={})",
      inline_layout.height,
      display_layout.height
    );
  });
}

#[test]
fn fraction_linethickness_zero_emits_no_rule_fragments() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let dom = renderer
      .parse_html("<math><mfrac linethickness=\"0\"><mi>a</mi><mi>b</mi></mfrac></math>")
      .expect("dom");
    let fragments = renderer
      .layout_document(&dom, 200, 200)
      .expect("layout document");

    let replaced = find_math_fragment(&fragments.root).expect("math fragment");
    let ReplacedType::Math(math) = replaced else {
      panic!("expected math replaced type");
    };
    let layout = math.layout.as_ref().expect("math layout");
    let rules = layout
      .fragments
      .iter()
      .filter(|f| matches!(f, MathFragment::Rule(_)))
      .count();
    assert_eq!(
      rules, 0,
      "linethickness=0 should not produce a fraction bar"
    );
  });
}

#[test]
fn math_constructs_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html =
      std::fs::read_to_string(fixture_path("math_constructs")).expect("load math_constructs");
    let png = renderer
      .render_to_png(&html, 360, 220)
      .expect("render math constructs");
    compare_golden("math_constructs", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_menclose_diagonal_strike_matches_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_menclose_diagonal_strike"))
      .expect("load math_menclose_diagonal_strike");
    let png = renderer
      .render_to_png(&html, 420, 200)
      .expect("render menclose diagonal strike");
    compare_golden(
      "math_menclose_diagonal_strike",
      &png,
      &CompareConfig::lenient(),
    );
  });
}

#[test]
fn math_table_alignment_matches_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_matrix")).expect("load math_matrix");
    let png = renderer
      .render_to_png(&html, 360, 220)
      .expect("render math matrix");
    compare_golden("math_matrix", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_table_spacing_matches_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html =
      std::fs::read_to_string(fixture_path("math_table_spacing")).expect("load math_table_spacing");
    let png = renderer
      .render_to_png(&html, 360, 220)
      .expect("render math table spacing");
    compare_golden("math_table_spacing", &png, &CompareConfig::lenient());
  });
}

#[test]
fn inline_math_baseline_matches_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_inline")).expect("load math_inline");
    let png = renderer
      .render_to_png(&html, 420, 220)
      .expect("render inline math");
    compare_golden("math_inline", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_stretchy_ops_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html =
      std::fs::read_to_string(fixture_path("math_stretchy_ops")).expect("load math_stretchy_ops");
    let png = renderer
      .render_to_png(&html, 540, 360)
      .expect("render stretchy math");
    compare_golden("math_stretchy_ops", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_ms_quotes_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_ms_quotes")).expect("load math_ms_quotes");
    let png = renderer
      .render_to_png(&html, 240, 120)
      .expect("render math ms quotes");
    compare_golden("math_ms_quotes", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_accents_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer_with_fixture_fonts();
    let html = std::fs::read_to_string(fixture_path("math_accents")).expect("load math_accents");
    let png = renderer
      .render_to_png(&html, 560, 260)
      .expect("render math accents");
    compare_golden("math_accents", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_fractions_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html =
      std::fs::read_to_string(fixture_path("math_fractions")).expect("load math_fractions");
    let png = renderer
      .render_to_png(&html, 520, 420)
      .expect("render math fractions");
    compare_golden("math_fractions", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_operator_spacing_matches_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_operator_spacing"))
      .expect("load math_operator_spacing");
    let png = renderer
      .render_to_png(&html, 520, 260)
      .expect("render math operator spacing");
    compare_golden("math_operator_spacing", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_operator_dictionary_defaults_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_operator_dictionary_defaults"))
      .expect("load math_operator_dictionary_defaults");
    let png = renderer
      .render_to_png(&html, 560, 320)
      .expect("render math operator dictionary defaults");
    compare_golden(
      "math_operator_dictionary_defaults",
      &png,
      &CompareConfig::lenient(),
    );
  });
}

#[test]
fn math_mfenced_separators_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_mfenced_separators"))
      .expect("load math_mfenced_separators");
    let png = renderer
      .render_to_png(&html, 360, 200)
      .expect("render mfenced separators");
    compare_golden("math_mfenced_separators", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_scriptlevel_matches_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html =
      std::fs::read_to_string(fixture_path("math_scriptlevel")).expect("load math_scriptlevel");
    let png = renderer
      .render_to_png(&html, 520, 260)
      .expect("render math scriptlevel");
    compare_golden("math_scriptlevel", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_mathvariant_alphanumerics_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_mathvariant_alphanumerics"))
      .expect("load math_mathvariant_alphanumerics");
    let png = renderer
      .render_to_png(&html, 560, 240)
      .expect("render math mathvariant alphanumerics");
    compare_golden("math_mathvariant_alphanumerics", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_displaystyle_limits_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_displaystyle_limits"))
      .expect("load math_displaystyle_limits");
    let png = renderer
      .render_to_png(&html, 520, 300)
      .expect("render math displaystyle limits");
    compare_golden("math_displaystyle_limits", &png, &CompareConfig::lenient());
  });
}

#[test]
fn math_semantics_annotations_ignored_match_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html = std::fs::read_to_string(fixture_path("math_semantics_wikipedia_like"))
      .expect("load math_semantics_wikipedia_like");
    let png = renderer
      .render_to_png(&html, 240, 160)
      .expect("render math semantics");
    compare_golden(
      "math_semantics_wikipedia_like",
      &png,
      &CompareConfig::lenient(),
    );
  });
}

fn fixture_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/html/{}.html", name))
}

fn golden_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/golden/{}.png", name))
}

fn compare_golden(name: &str, rendered_png: &[u8], config: &CompareConfig) {
  let golden = golden_path(name);
  if std::env::var("UPDATE_GOLDEN").is_ok() {
    std::fs::write(&golden, rendered_png).expect("write golden");
  }
  let expected = std::fs::read(&golden).expect("golden image");
  let rendered_pixmap = load_png_from_bytes(rendered_png).expect("rendered png");
  let expected_pixmap = load_png_from_bytes(&expected).expect("expected png");
  let diff = compare_images(&rendered_pixmap, &expected_pixmap, config);
  assert!(
    diff.is_match(),
    "golden {} mismatch: {}",
    name,
    diff.summary()
  );
}

#[test]
fn sqrt_and_scripts_produce_nonzero_layout() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let dom = renderer
      .parse_html("<math><msqrt><msubsup><mi>x</mi><mi>i</mi><mi>2</mi></msubsup></msqrt></math>")
      .expect("dom");
    let fragments = renderer
      .layout_document(&dom, 300, 200)
      .expect("layout document");

    let replaced = find_math_fragment(&fragments.root).expect("math fragment");
    let ReplacedType::Math(math) = replaced else {
      panic!("expected math replaced type");
    };
    let layout = math.layout.as_ref().expect("math layout");
    assert!(layout.height > 0.0, "height should be positive");
    assert!(layout.width > 0.0);
  });
}

#[test]
fn math_layout_falls_back_without_fonts() {
  let style = fastrender::ComputedStyle::default();
  let node = MathNode::Fraction {
    numerator: Box::new(MathNode::Identifier {
      text: "x".into(),
      variant: Some(MathVariant::Italic),
    }),
    denominator: Box::new(MathNode::Number {
      text: "2".into(),
      variant: None,
    }),
    linethickness: None,
    bevelled: false,
    numalign: ColumnAlign::Center,
    denomalign: ColumnAlign::Center,
  };
  let layout = layout_mathml(&node, &style, &FontContext::empty());
  assert!(layout.width > 0.0);
  assert!(layout.height > 0.0);
  assert!(layout.baseline > 0.0 && layout.baseline < layout.height);
}

fn math_font_context() -> FontContext {
  let mut cfg = FontConfig::default();
  cfg.font_dirs.push(PathBuf::from("tests/fixtures/fonts"));
  FontContext::with_config(cfg)
}

#[test]
fn scripts_on_italic_identifiers_offset_and_raise() {
  let ctx = math_font_context();
  let mut style = fastrender::ComputedStyle::default();
  style.font_size = 24.0;
  style.font_family = vec!["STIX Two Math".to_string()].into();
  let base_node = MathNode::Identifier {
    text: "f".into(),
    variant: Some(MathVariant::Italic),
  };
  let sup_node = MathNode::Identifier {
    text: "i".into(),
    variant: None,
  };
  let node = MathNode::Superscript {
    base: Box::new(base_node.clone()),
    superscript: Box::new(sup_node),
  };
  let base_layout = layout_mathml(&base_node, &style, &ctx);
  let combined = layout_mathml(&node, &style, &ctx);
  let base_fragments = base_layout.fragments.len();
  let (sup_origin, sup_advance) = combined
    .fragments
    .get(base_fragments)
    .and_then(|f| match f {
      fastrender::math::MathFragment::Glyph { origin, run } => Some((*origin, run.advance)),
      _ => None,
    })
    .expect("superscript fragment");
  assert!(
    combined.width > base_layout.width,
    "superscript should extend total width beyond the base glyph"
  );
  assert!(
    sup_origin.y < combined.baseline,
    "superscript glyph origin should be above the main baseline (origin.y={}, baseline={})",
    sup_origin.y,
    combined.baseline
  );
}

#[test]
fn radicals_scale_with_nested_content() {
  let ctx = math_font_context();
  let mut style = fastrender::ComputedStyle::default();
  style.font_size = 24.0;
  style.font_family = vec!["STIX Two Math".to_string()].into();
  let radicand = MathNode::SubSuperscript {
    base: Box::new(MathNode::Identifier {
      text: "x".into(),
      variant: Some(MathVariant::Italic),
    }),
    subscript: Box::new(MathNode::Number {
      text: "2".into(),
      variant: None,
    }),
    superscript: Box::new(MathNode::Identifier {
      text: "n".into(),
      variant: None,
    }),
  };
  let sqrt_node = MathNode::Sqrt(Box::new(radicand.clone()));
  let radicand_layout = layout_mathml(&radicand, &style, &ctx);
  let sqrt_layout = layout_mathml(&sqrt_node, &style, &ctx);
  assert!(
    sqrt_layout.height > radicand_layout.height + style.font_size * 0.1,
    "sqrt should add headroom above a scripted radicand"
  );
  assert!(
    sqrt_layout.width > radicand_layout.width,
    "sqrt layout should extend to cover the radicand"
  );
}

#[test]
fn matrix_table_aligns_cells() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let dom = renderer
      .parse_html(
        "<math><mtable><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr><mtr><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mtr></mtable></math>",
      )
      .expect("dom");
    let fragments = renderer
      .layout_document(&dom, 400, 200)
      .expect("layout document");

    let replaced = find_math_fragment(&fragments.root).expect("math fragment");
    let ReplacedType::Math(math) = replaced else {
      panic!("expected math replaced type");
    };
    let layout = math.layout.as_ref().expect("math layout");
    assert!(layout.width > 0.0 && layout.height > 0.0);
    assert!(
      layout.fragments.len() >= 4,
      "matrix should have at least four glyph fragments"
    );
  });
}

#[test]
fn math_piecewise_construct_matches_golden() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let html =
      std::fs::read_to_string(fixture_path("math_piecewise")).expect("load math_piecewise");
    let png = renderer
      .render_to_png(&html, 560, 360)
      .expect("render math piecewise");
    compare_golden("math_piecewise", &png, &CompareConfig::lenient());
  });
}

#[test]
fn munderover_stacks_limits() {
  with_stack(|| {
    let mut renderer = deterministic_renderer();
    let base_dom = renderer
      .parse_html("<math><mo>&#8721;</mo></math>")
      .expect("base dom");
    let base_layout = {
      let base_fragments = renderer
        .layout_document(&base_dom, 200, 200)
        .expect("layout base");
      let replaced = find_math_fragment(&base_fragments.root).expect("math fragment");
      let ReplacedType::Math(math) = replaced else {
        panic!("expected math replaced type");
      };
      math.layout.as_ref().expect("layout").as_ref().clone()
    };

    let inline_dom = renderer
      .parse_html("<math><munderover><mo>&#8721;</mo><mi>i</mi><mi>n</mi></munderover></math>")
      .expect("inline dom");
    let inline_layout = {
      let fragments = renderer
        .layout_document(&inline_dom, 240, 240)
        .expect("layout inline");
      let replaced = find_math_fragment(&fragments.root).expect("math fragment");
      let ReplacedType::Math(math) = replaced else {
        panic!("expected math replaced type");
      };
      math.layout.as_ref().expect("math layout").as_ref().clone()
    };

    let display_dom = renderer
      .parse_html(
        "<math display=\"block\"><munderover><mo>&#8721;</mo><mi>i</mi><mi>n</mi></munderover></math>",
      )
      .expect("display dom");
    let display_layout = {
      let fragments = renderer
        .layout_document(&display_dom, 240, 240)
        .expect("layout display");
      let replaced = find_math_fragment(&fragments.root).expect("math fragment");
      let ReplacedType::Math(math) = replaced else {
        panic!("expected math replaced type");
      };
      math.layout.as_ref().expect("math layout").as_ref().clone()
    };

    // Inline style should use movable limits (scripts), increasing width.
    assert!(inline_layout.width > base_layout.width);
    // Display style should stack limits, increasing height versus inline.
    assert!(display_layout.height > inline_layout.height);
    assert!(display_layout.fragments.len() >= 3);
  });
}

#[test]
fn operator_spacing_increases_layout_width() {
  let ctx = math_font_context();
  let mut style = fastrender::ComputedStyle::default();
  style.font_size = 24.0;
  style.font_family = vec!["STIX Two Math".to_string()].into();
  let make_expr = |lspace: Option<MathLengthOrKeyword>, rspace: Option<MathLengthOrKeyword>| {
    MathNode::Row(vec![
      MathNode::Identifier {
        text: "x".into(),
        variant: Some(MathVariant::Italic),
      },
      MathNode::Operator {
        text: "+".into(),
        form: None,
        stretchy: None,
        lspace,
        rspace,
        variant: None,
      },
      MathNode::Identifier {
        text: "y".into(),
        variant: Some(MathVariant::Italic),
      },
    ])
  };
  let spaced = layout_mathml(&make_expr(None, None), &style, &ctx);
  let tight = layout_mathml(
    &make_expr(
      Some(MathLengthOrKeyword::Zero),
      Some(MathLengthOrKeyword::Zero),
    ),
    &style,
    &ctx,
  );
  assert!(
    spaced.width > tight.width + style.font_size * 0.2,
    "default operator spacing should increase width: {} vs {}",
    spaced.width,
    tight.width
  );
}
