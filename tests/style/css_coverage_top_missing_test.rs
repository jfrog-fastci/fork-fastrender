use fastrender::css::parser::parse_stylesheet;
use fastrender::css::supports::supports_declaration;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::{AlignContent, AlignItems, FlexBasis, FlexWrap, JustifyContent, TextAlign};
use fastrender::style::values::Length;

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

fn styled_root(html: &str) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0))
}

fn styles_for_first(
  html: &str,
  tag: &str,
) -> std::sync::Arc<fastrender::style::ComputedStyle> {
  let styled = styled_root(html);
  find_first(&styled, tag)
    .unwrap_or_else(|| panic!("expected to find <{tag}>"))
    .styles
    .clone()
}

fn styles_for_div_and_span(
  html: &str,
) -> (
  std::sync::Arc<fastrender::style::ComputedStyle>,
  std::sync::Arc<fastrender::style::ComputedStyle>,
) {
  let styled = styled_root(html);
  let div = find_first(&styled, "div").expect("div");
  let span = find_first(div, "span").expect("span");
  (div.styles.clone(), span.styles.clone())
}

#[test]
fn ms_flex_order_sets_order() {
  let (_div, span) = styles_for_div_and_span(
    r#"<div style="display:flex"><span style="-ms-flex-order:2"></span></div>"#,
  );
  assert_eq!(span.order, 2);
}

#[test]
fn ms_flex_pack_justify_maps_to_space_between() {
  let (div, _span) = styles_for_div_and_span(
    r#"<div style="display:flex;-ms-flex-pack:justify"><span></span></div>"#,
  );
  assert_eq!(div.justify_content, JustifyContent::SpaceBetween);
}

#[test]
fn ms_flex_pack_distribute_maps_to_space_around() {
  let (div, _span) = styles_for_div_and_span(
    r#"<div style="display:flex;-ms-flex-pack:distribute"><span></span></div>"#,
  );
  assert_eq!(div.justify_content, JustifyContent::SpaceAround);
}

#[test]
fn ms_flex_pack_left_maps_to_flex_start() {
  let (div, _span) = styles_for_div_and_span(r#"<div style="display:flex;-ms-flex-pack:left"><span></span></div>"#);
  assert_eq!(div.justify_content, JustifyContent::FlexStart);
}

#[test]
fn ms_flex_pack_space_evenly_maps_to_space_evenly() {
  let (div, _span) = styles_for_div_and_span(
    r#"<div style="display:flex;-ms-flex-pack:space-evenly"><span></span></div>"#,
  );
  assert_eq!(div.justify_content, JustifyContent::SpaceEvenly);
}

#[test]
fn ms_flex_pack_between_maps_to_space_between() {
  let (div, _span) = styles_for_div_and_span(
    r#"<div style="display:flex;-ms-flex-pack:between"><span></span></div>"#,
  );
  assert_eq!(div.justify_content, JustifyContent::SpaceBetween);
}

#[test]
fn ms_flex_align_center_maps_to_align_items_center() {
  let (div, _span) = styles_for_div_and_span(
    r#"<div style="display:flex;-ms-flex-align:center"><span></span></div>"#,
  );
  assert_eq!(div.align_items, AlignItems::Center);
}

#[test]
fn ms_flex_line_pack_left_maps_to_align_content_flex_start() {
  let div = styles_for_first(
    r#"<div style="display:flex;-ms-flex-line-pack:left"></div>"#,
    "div",
  );
  assert_eq!(div.align_content, AlignContent::FlexStart);
}

#[test]
fn align_content_left_and_right_are_accepted_as_legacy_aliases() {
  let div_left = styles_for_first(r#"<div style="display:flex;align-content:left"></div>"#, "div");
  assert_eq!(div_left.align_content, AlignContent::Start);

  let div_right = styles_for_first(r#"<div style="display:flex;align-content:right"></div>"#, "div");
  assert_eq!(div_right.align_content, AlignContent::End);
}

#[test]
fn ms_flex_wrap_aliases_map_to_flex_wrap() {
  let div_wrap = styles_for_first(r#"<div style="display:flex;-ms-flex-wrap:wrap"></div>"#, "div");
  assert_eq!(div_wrap.flex_wrap, FlexWrap::Wrap);

  // `none` is the legacy IE10 spelling for `nowrap`. Ensure it can override earlier declarations.
  let div_none = styles_for_first(
    r#"<div style="display:flex;flex-wrap:wrap;-ms-flex-wrap:none"></div>"#,
    "div",
  );
  assert_eq!(div_none.flex_wrap, FlexWrap::NoWrap);
}

#[test]
fn supports_legacy_alignment_keywords_seen_in_fixtures() {
  assert!(supports_declaration("align-content", "left"));
  assert!(supports_declaration("align-content", "right"));
  assert!(supports_declaration("-ms-flex-line-pack", "left"));
  assert!(supports_declaration("-ms-flex-pack", "space-evenly"));
  assert!(supports_declaration("-ms-flex-pack", "between"));
  assert!(supports_declaration("-ms-flex-wrap", "none"));
}

#[test]
fn ms_flex_item_align_center_maps_to_align_self_center() {
  let (_div, span) = styles_for_div_and_span(
    r#"<div style="display:flex"><span style="-ms-flex-item-align:center"></span></div>"#,
  );
  assert_eq!(span.align_self, Some(AlignItems::Center));
}

#[test]
fn ms_flex_positive_and_negative_map_to_flex_grow_and_shrink() {
  let (_div, span) = styles_for_div_and_span(
    r#"<div style="display:flex"><span style="-ms-flex-positive:2;-ms-flex-negative:3"></span></div>"#,
  );
  assert_eq!(span.flex_grow, 2.0);
  assert_eq!(span.flex_shrink, 3.0);
}

#[test]
fn ms_flex_preferred_size_maps_to_flex_basis() {
  let (_div, span) = styles_for_div_and_span(
    r#"<div style="display:flex"><span style="-ms-flex-preferred-size:10px"></span></div>"#,
  );
  assert_eq!(span.flex_basis, FlexBasis::Length(Length::px(10.0)));
}

#[test]
fn ms_grid_row_and_column_map_to_grid_placement_raw() {
  let (_div, span) = styles_for_div_and_span(
    r#"<div style="display:grid"><span style="-ms-grid-row:2;-ms-grid-column:3"></span></div>"#,
  );
  assert_eq!(span.grid_row_raw.as_deref(), Some("2"));
  assert_eq!(span.grid_column_raw.as_deref(), Some("3"));
}

#[test]
fn ms_grid_span_properties_combine_with_ms_grid_row_and_column() {
  let (_div, span) = styles_for_div_and_span(
    r#"<div style="display:grid"><span style="-ms-grid-row:2;-ms-grid-row-span:3;-ms-grid-column:1;-ms-grid-column-span:2"></span></div>"#,
  );
  assert_eq!(span.grid_row_raw.as_deref(), Some("2 / span 3"));
  assert_eq!(span.grid_column_raw.as_deref(), Some("1 / span 2"));
}

#[test]
fn ms_grid_span_properties_can_appear_before_start() {
  let (_div, span) = styles_for_div_and_span(
    r#"<div style="display:grid"><span style="-ms-grid-row-span:3;-ms-grid-row:2;-ms-grid-column-span:2;-ms-grid-column:1"></span></div>"#,
  );
  assert_eq!(span.grid_row_raw.as_deref(), Some("2 / span 3"));
  assert_eq!(span.grid_column_raw.as_deref(), Some("1 / span 2"));
}

#[test]
fn justify_content_right_is_accepted_as_legacy_alias() {
  let div = styles_for_first(r#"<div style="display:flex;justify-content:right"></div>"#, "div");
  assert_eq!(div.justify_content, JustifyContent::End);
}

#[test]
fn justify_content_stretch_is_supported() {
  let div = styles_for_first(r#"<div style="display:grid;justify-content:stretch"></div>"#, "div");
  assert_eq!(div.justify_content, JustifyContent::Stretch);
}

#[test]
fn supports_border_color_shorthand_multiple_values() {
  assert!(supports_declaration("border-color", "red green blue"));
  assert!(!supports_declaration("border-color", "red green blue black orange"));
}

#[test]
fn supports_overflow_shorthand_two_values() {
  assert!(supports_declaration("overflow", "hidden scroll"));
  assert!(!supports_declaration("overflow", "hidden scroll auto"));
}

#[test]
fn supports_text_align_webkit_match_parent_alias() {
  assert!(supports_declaration("text-align", "-webkit-match-parent"));
}

#[test]
fn text_align_webkit_match_parent_resolves_like_match_parent() {
  let html = r#"<div style="text-align:end"><span style="text-align:-webkit-match-parent"></span></div>"#;
  let styled = styled_root(html);
  let div = find_first(&styled, "div").expect("div");
  let span = find_first(div, "span").expect("span");
  assert_eq!(div.styles.text_align, TextAlign::End);
  // `match-parent` should resolve to a physical value based on the parent direction.
  assert_eq!(span.styles.text_align, TextAlign::Right);
}
