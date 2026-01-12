use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::BackgroundBox;
use fastrender::style::types::GridTrack;

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

fn find_first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_first(child, tag) {
      return Some(found);
    }
  }
  None
}

fn render_div_display(css: &str) -> String {
  let dom = dom::parse_html(r#"<div></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let div = find_first(&styled, "div").expect("div");
  display(div)
}

fn render_div_background_clip(css: &str) -> BackgroundBox {
  let dom = dom::parse_html(r#"<div></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let div = find_first(&styled, "div").expect("div");
  div
    .styles
    .background_layers
    .first()
    .expect("background layer")
    .clip
}

#[test]
fn supports_declaration_matches() {
  let css = r"@supports (display: grid) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_container_type_scroll_state_matches() {
  // Tailwind v4 (shopify.com pageset fixture) gates container scroll-state utilities behind
  // `@supports (container-type: scroll-state)`.
  let css = r"@supports (container-type: scroll-state) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_container_type_size_scroll_state_matches() {
  let css = r"@supports (container-type: size scroll-state) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_container_type_inline_size_scroll_state_matches() {
  let css = r"@supports (container-type: inline-size scroll-state) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_container_type_scroll_state_size_matches() {
  let css = r"@supports (container-type: scroll-state size) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_container_type_scroll_state_inline_size_matches() {
  let css = r"@supports (container-type: scroll-state inline-size) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_container_shorthand_scroll_state_matches() {
  let css = r"@supports (container: demo / scroll-state) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_container_type_invalid_combo_is_unsupported() {
  let css = r"@supports (container-type: normal scroll-state) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_not_negates() {
  let css = r"@supports not (display: grid) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_not_ms_grid_display_matches() {
  // Modern browsers do not support the legacy IE `-ms-grid` display keyword, so `not` should match.
  // Real-world sites (e.g. ft.com) use this pattern to gate responsive grid templates.
  let css = r"@supports not (display: -ms-grid) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_not_ms_grid_matches_modern_browser_behavior() {
  // Many sites gate modern grid syntax behind `@supports not (display: -ms-grid)` to exclude
  // legacy IE/EdgeHTML. Our cascade should treat `display: -ms-grid` as unsupported in support
  // queries so the modern branch participates in the cascade (Chrome behavior).
  let dom = dom::parse_html(r#"<div class="slice"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#"
      .slice { display: grid; grid-template-columns: 1fr 1260px 1fr; }
      @supports not (display: -ms-grid) {
        .slice { grid-template-columns: 1fr minmax(auto, 1260px) 1fr; }
      }
    "#,
  )
  .unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let div = find_first(&styled, "div").expect("div");

  assert_eq!(div.styles.grid_template_columns.len(), 3);
  match &div.styles.grid_template_columns[1] {
    GridTrack::MinMax(min, max) => {
      assert!(matches!(&**min, GridTrack::Auto));
      assert!(matches!(&**max, GridTrack::Length(len) if len.value == 1260.0));
    }
    other => panic!("expected minmax() track, got: {other:?}"),
  }
}

#[test]
fn supports_nested_conditions_combine_correctly() {
  let css = r"@supports ((display: grid) and (color: red)) or (selector(:has(*))) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_device_cmyk_color_function() {
  let css = r"@supports (color: device-cmyk(0 1 1 0)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_system_color_keywords_match() {
  let css = r"@supports (color: Canvas) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_dynamic_range_limit_mix_in_color_positions() {
  let css = r"@supports (color: dynamic-range-limit-mix(in srgb-linear, red, blue)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_unknown_color_keywords_are_unsupported() {
  let css = r"@supports (color: NotARealColorKeyword) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_and_inside_url_is_treated_as_value() {
  let css = r"@supports (background: url(and.png)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_background_clip_text() {
  let css = r"@supports (background-clip: text) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_vendor_prefixed_properties_match_when_unprefixed_supported() {
  let css = r"@supports (-webkit-transform: rotate(10deg)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");

  let css = r"@supports (-moz-transform: rotate(10deg)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");

  let css = r"@supports (-o-transform: rotate(10deg)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");

  let css = r"@supports (-ms-transform: rotate(10deg)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_webkit_logical_margin_start_and_padding_end_are_supported() {
  let css = r"@supports (-webkit-margin-start: 10px) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");

  let css = r"@supports (-webkit-padding-end: 5px) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_ms_flex_line_pack_is_supported_and_rejects_invalid_keywords() {
  let css = r"@supports (-ms-flex-line-pack: justify) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");

  let css = r"@supports (-ms-flex-line-pack: bogus) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_vendor_prefixed_unknown_properties_are_unsupported() {
  let css = r"@supports (-webkit-not-a-property: 1) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn webkit_background_clip_text_is_aliased() {
  let css = r"div { -webkit-background-clip: text; }";
  assert_eq!(render_div_background_clip(css), BackgroundBox::Text);
}

#[test]
fn moz_background_clip_content_is_aliased() {
  let css = r"div { -moz-background-clip: content; }";
  assert_eq!(render_div_background_clip(css), BackgroundBox::ContentBox);
}

#[test]
fn moz_background_clip_padding_is_aliased() {
  let css = r"div { -moz-background-clip: padding; }";
  assert_eq!(render_div_background_clip(css), BackgroundBox::PaddingBox);
}

#[test]
fn supports_vendor_prefixed_properties_are_case_insensitive() {
  let css = r"@supports (-WeBkIt-TrAnSfOrM: rotate(10deg)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_vendor_prefixed_backdrop_filter_matches() {
  let css = r"@supports (-webkit-backdrop-filter: blur(1px)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_legacy_ms_grid_properties_are_supported() {
  let css = r"@supports (-ms-grid-row: 1) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_legacy_ms_grid_properties_require_positive_integer_values() {
  let css = r"@supports (-ms-grid-row: 0) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
  let css = r"@supports (-ms-grid-column-span: 0) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_webkit_calc_lengths() {
  let css = r"@supports (max-block-size: -webkit-calc(100vh - 160px)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_font_format_woff2_matches() {
  let css = r"@supports font-format(woff2) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_font_tech_color_colrv1_matches() {
  let css = r"@supports font-tech(color-COLRv1) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_font_tech_list_requires_all_techs() {
  let css = r"@supports font-tech(variations, unknown-tech) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_font_format_list_matches_if_any_format_supported() {
  let css = r"@supports font-format(zebra, woff2) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_font_queries_combine_with_declarations_and_selectors() {
  let css = r"@supports (display: grid) and font-format(woff2) and selector(:has(*)) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_unknown_font_keywords_are_false() {
  let css = r"@supports font-format(zebra) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");

  let css = r"@supports font-tech(color-zebra) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_font_format_string_arguments_are_unsupported() {
  // CSS Conditional 5 defines font-format() as supported only for keyword arguments; string
  // arguments should always evaluate false.
  let css = r#"@supports font-format("woff2") { div { display: inline; } }"#;
  assert_eq!(render_div_display(css), "block");

  let css = r#"@supports not font-format("woff2") { div { display: inline; } }"#;
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_font_tech_string_arguments_do_not_match_keywords() {
  // `font-tech()` does not accept string arguments. We parse it forgivingly but treat it as
  // unsupported so `not font-tech(\"...\")` can still match.
  let css = r#"@supports font-tech("variations") { div { display: inline; } }"#;
  assert_eq!(render_div_display(css), "block");

  let css = r#"@supports not font-tech("variations") { div { display: inline; } }"#;
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_at_rule_layer_matches() {
  let css = r"@supports at-rule(@layer) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_at_rule_container_matches() {
  let css = r"@supports at-rule(@container) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_at_rule_property_matches() {
  let css = r"@supports at-rule(@property) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_not_at_rule_unknown_matches() {
  let css = r"@supports not at-rule(@unknown-at-rule) { div { display: inline; } }";
  assert_eq!(render_div_display(css), "inline");
}
