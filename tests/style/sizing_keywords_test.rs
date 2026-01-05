use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::IntrinsicSizeKeyword;
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

fn styled_div(html: &str) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  find_first(&styled, "div").expect("div").clone()
}

fn styled_spans(html: &str) -> Vec<StyledNode> {
  let container = styled_div(html);
  container
    .children
    .iter()
    .filter(|child| {
      child
        .node
        .tag_name()
        .map(|t| t.eq_ignore_ascii_case("span"))
        .unwrap_or(false)
    })
    .cloned()
    .collect()
}

#[test]
fn intrinsic_sizing_keywords_parse_for_width() {
  let node = styled_div(r#"<div style="width: max-content"></div>"#);
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::MaxContent)
  );

  let node = styled_div(r#"<div style="width: -webkit-max-content"></div>"#);
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::MaxContent)
  );

  let node = styled_div(r#"<div style="width: min-content"></div>"#);
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::MinContent)
  );
}

#[test]
fn fit_content_keyword_and_function_parse_for_width() {
  let node = styled_div(r#"<div style="width: fit-content"></div>"#);
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::FitContent { limit: None })
  );

  let node = styled_div(r#"<div style="width: -moz-fit-content"></div>"#);
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::FitContent { limit: None })
  );

  let node = styled_div(r#"<div style="width: fit-content(10px)"></div>"#);
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::px(10.0))
    })
  );

  let node = styled_div(r#"<div style="width: -webkit-fit-content(50%)"></div>"#);
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::percent(50.0))
    })
  );
}

#[test]
fn intrinsic_sizing_keywords_parse_for_max_height() {
  let node = styled_div(r#"<div style="max-height: max-content"></div>"#);
  assert_eq!(node.styles.max_height, None);
  assert_eq!(
    node.styles.max_height_keyword,
    Some(IntrinsicSizeKeyword::MaxContent)
  );
}

#[test]
fn inline_size_maps_to_physical_axes_and_respects_declaration_order() {
  let spans = styled_spans(
    r#"<div>
        <span style="width: 10px; inline-size: max-content"></span>
        <span style="inline-size: max-content; width: 10px"></span>
      </div>"#,
  );
  assert_eq!(spans.len(), 2);
  assert_eq!(spans[0].styles.width, None);
  assert_eq!(
    spans[0].styles.width_keyword,
    Some(IntrinsicSizeKeyword::MaxContent)
  );
  assert_eq!(spans[1].styles.width, Some(Length::px(10.0)));
  assert_eq!(spans[1].styles.width_keyword, None);

  let node = styled_div(
    r#"<div style="writing-mode: vertical-rl; inline-size: 10px; block-size: max-content"></div>"#,
  );
  // Inline axis is vertical in vertical writing modes, so inline-size maps to height.
  assert_eq!(node.styles.height, Some(Length::px(10.0)));
  assert_eq!(node.styles.height_keyword, None);
  // Block axis is horizontal, so block-size maps to width.
  assert_eq!(node.styles.width, None);
  assert_eq!(
    node.styles.width_keyword,
    Some(IntrinsicSizeKeyword::MaxContent)
  );
}

#[test]
fn min_max_logical_sizes_map_to_physical_properties() {
  let node = styled_div(r#"<div style="max-inline-size: fit-content(20px)"></div>"#);
  assert_eq!(node.styles.max_width, None);
  assert_eq!(
    node.styles.max_width_keyword,
    Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::px(20.0))
    })
  );

  let node = styled_div(r#"<div style="min-block-size: min-content"></div>"#);
  assert_eq!(node.styles.min_height, None);
  assert_eq!(
    node.styles.min_height_keyword,
    Some(IntrinsicSizeKeyword::MinContent)
  );
}
