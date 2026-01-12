use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::inline::InlineFormattingContext;
use crate::style::float::Float;
use crate::style::types::CaseTransform;
use crate::style::types::Direction;
use crate::style::types::TextTransform;
use crate::style::values::Length;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::Rgba;
use crate::{BoxNode, FormattingContextType};
use std::sync::Arc;

fn find_first_line<'a>(
  fragment: &'a crate::FragmentNode,
) -> Option<&'a crate::FragmentNode> {
  if matches!(fragment.content, crate::FragmentContent::Line { .. }) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_first_line(child) {
      return Some(found);
    }
  }
  None
}

fn collect_texts<'a>(fragment: &'a crate::FragmentNode, out: &mut Vec<(&'a str, Rgba)>) {
  if let crate::FragmentContent::Text {
    text, is_marker, ..
  } = &fragment.content
  {
    if !is_marker {
      let color = fragment
        .style
        .as_ref()
        .map(|s| s.color)
        .unwrap_or_else(|| crate::Rgba::BLACK);
      out.push((text.as_ref(), color));
    }
  }
  for child in fragment.children.iter() {
    collect_texts(child, out);
  }
}

fn collect_text_fragments<'a>(
  fragment: &'a crate::FragmentNode,
  out: &mut Vec<&'a crate::FragmentNode>,
) {
  if matches!(fragment.content, crate::FragmentContent::Text { .. }) {
    out.push(fragment);
  }
  for child in fragment.children.iter() {
    collect_text_fragments(child, out);
  }
}

fn find_fragment_with_background<'a>(
  fragment: &'a crate::FragmentNode,
  color: Rgba,
) -> Option<&'a crate::FragmentNode> {
  if fragment
    .style
    .as_ref()
    .map(|s| s.background_color == color)
    .unwrap_or(false)
  {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_with_background(child, color) {
      return Some(found);
    }
  }
  None
}

#[test]
fn first_letter_and_first_line_styles_apply_to_fragments() {
  let mut container_style = ComputedStyle::default();
  container_style.display = crate::style::display::Display::Block;
  let mut first_line_style = ComputedStyle::default();
  first_line_style.color = Rgba::rgb(0, 0, 255);
  first_line_style.text_transform = TextTransform::with_case(CaseTransform::Uppercase);

  let mut first_letter_style = ComputedStyle::default();
  first_letter_style.padding_right = Length::px(4.0);
  first_letter_style.margin_right = Some(Length::px(8.0));
  first_letter_style.background_color = Rgba::rgb(250, 200, 200);
  first_letter_style.color = Rgba::rgb(200, 0, 0);

  let text_style = Arc::new(ComputedStyle::default());
  let text = BoxNode::new_text(text_style.clone(), "hello world".to_string());

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Inline,
    vec![text],
  );
  container.first_line_style = Some(Arc::new(first_line_style.clone()));
  container.first_letter_style = Some(Arc::new(first_letter_style.clone()));

  let ifc = InlineFormattingContext::new();
  let fragment = ifc
    .layout(&container, &LayoutConstraints::definite_width(180.0))
    .expect("inline layout");

  let mut all_texts = Vec::new();
  collect_texts(&fragment, &mut all_texts);
  assert!(
    all_texts
      .iter()
      .any(|(t, color)| t.to_lowercase().contains('h') && *color == first_letter_style.color),
    "first-letter color should be applied to the initial character",
  );

  let first_line = find_first_line(&fragment).expect("line fragment");
  let mut first_line_texts = Vec::new();
  collect_texts(first_line, &mut first_line_texts);
  assert!(
    first_line_texts
      .iter()
      .any(|(t, color)| t.to_lowercase().contains("ello") && *color == first_line_style.color),
    "first-line style should color the remaining text"
  );
}

#[test]
fn first_letter_wraps_punctuation_and_combining_marks() {
  let mut container_style = ComputedStyle::default();
  container_style.display = crate::style::display::Display::Block;
  let mut first_letter_style = ComputedStyle::default();
  first_letter_style.color = Rgba::rgb(10, 20, 30);

  let text_style = Arc::new(ComputedStyle::default());
  let text = BoxNode::new_text(text_style.clone(), "\u{201c}A\u{0301}bc".to_string());

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Inline,
    vec![text],
  );
  container.first_letter_style = Some(Arc::new(first_letter_style));

  let ifc = InlineFormattingContext::new();
  let fragment = ifc
    .layout(&container, &LayoutConstraints::definite_width(200.0))
    .expect("inline layout");

  let mut texts = Vec::new();
  collect_texts(&fragment, &mut texts);
  assert!(
    texts
      .first()
      .map(|(t, _)| *t == "\u{201c}A\u{0301}")
      .unwrap_or(false),
    "first-letter pseudo should wrap leading punctuation and combining marks"
  );
  assert!(texts.iter().any(|(t, _)| *t == "bc"));
}

#[test]
fn first_letter_respects_rtl_direction() {
  let mut container_style = ComputedStyle::default();
  container_style.display = crate::style::display::Display::Block;
  container_style.direction = Direction::Rtl;
  let mut first_letter_style = ComputedStyle::default();
  first_letter_style.color = Rgba::rgb(200, 10, 10);

  let text_style = Arc::new(ComputedStyle::default());
  let text = BoxNode::new_text(text_style.clone(), "אבג".to_string());
  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Inline,
    vec![text],
  );
  container.first_letter_style = Some(Arc::new(first_letter_style));

  let ifc = InlineFormattingContext::new();
  let fragment = ifc
    .layout(&container, &LayoutConstraints::definite_width(200.0))
    .expect("inline layout");

  let mut texts = Vec::new();
  collect_texts(&fragment, &mut texts);
  assert!(
    texts.first().map(|(t, _)| *t == "א").unwrap_or(false),
    "RTL first-letter should start from the logical beginning of the text"
  );
}

#[test]
fn first_letter_float_creates_separate_fragment() {
  let mut container_style = ComputedStyle::default();
  container_style.display = crate::style::display::Display::Block;
  let mut first_letter_style = ComputedStyle::default();
  first_letter_style.float = Float::Left;
  first_letter_style.padding_right = Length::px(6.0);
  first_letter_style.margin_right = Some(Length::px(4.0));
  first_letter_style.background_color = Rgba::rgb(250, 230, 210);

  let text_style = Arc::new(ComputedStyle::default());
  let text = BoxNode::new_text(text_style.clone(), "Floating letter demo".to_string());

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Inline,
    vec![text],
  );
  container.first_letter_style = Some(Arc::new(first_letter_style.clone()));

  let ifc = InlineFormattingContext::new();
  let fragment = ifc
    .layout(&container, &LayoutConstraints::definite_width(180.0))
    .expect("inline layout");

  let float_fragment =
    find_fragment_with_background(&fragment, first_letter_style.background_color)
      .expect("float fragment for first-letter");
  assert!(float_fragment.bounds.width() > 0.0);

  let mut texts = Vec::new();
  collect_text_fragments(&fragment, &mut texts);
  assert!(
    texts.len() > 1,
    "float should not discard remaining inline text"
  );
}
