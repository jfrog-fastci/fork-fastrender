use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::BreakBetween;
use fastrender::style::types::BreakInside;
use fastrender::style::types::Direction;
use fastrender::style::types::HyphensMode;
use fastrender::style::types::LineBreak;
use fastrender::style::types::LineHeight;
use fastrender::style::types::OverflowAnchor;
use fastrender::style::types::OverflowWrap;
use fastrender::style::types::RubyAlign;
use fastrender::style::types::RubyMerge;
use fastrender::style::types::RubyPosition;
use fastrender::style::types::ScrollBehavior;
use fastrender::style::types::TextAlign;
use fastrender::style::types::UnicodeBidi;
use fastrender::style::types::VerticalAlign;
use fastrender::style::types::WhiteSpace;
use fastrender::style::types::WordBreak;
use fastrender::style::values::Length;

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
fn text_keyword_values_are_ascii_case_insensitive() {
  let dom = dom::parse_html(
    r#"
      <div id="case"></div>
      <div id="lineheight"></div>
      <div id="break"></div>
      <div id="invalid"></div>
    "#,
  )
  .expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      #case {
        direction: RTL;
        unicode-bidi: ISOLATE;
        white-space: PRE;
        line-break: ANYWHERE;
        hyphens: AUTO;
        word-break: BREAK-ALL;
        overflow-wrap: BREAK-WORD;
        overflow-anchor: NONE;
        scroll-behavior: SMOOTH;
        text-align: CENTER;
        vertical-align: TOP;
        text-indent: 10px HANGING EACH-LINE;
        ruby-position: UNDER;
        ruby-align: CENTER;
        ruby-merge: COLLAPSE;
      }

      #lineheight {
        line-height: 2;
        line-height: NORMAL;
      }

      #break {
        break-before: AVOID;
        break-before: invalid;
        break-inside: AVOID-COLUMN;
      }

      #invalid { vertical-align: invalid; }
    "#,
  )
  .expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let case = find_by_id(&styled, "case").expect("case element");
  assert_eq!(case.styles.direction, Direction::Rtl);
  assert_eq!(case.styles.unicode_bidi, UnicodeBidi::Isolate);
  assert_eq!(case.styles.white_space, WhiteSpace::Pre);
  assert_eq!(case.styles.line_break, LineBreak::Anywhere);
  assert_eq!(case.styles.hyphens, HyphensMode::Auto);
  assert_eq!(case.styles.word_break, WordBreak::BreakAll);
  assert_eq!(case.styles.overflow_wrap, OverflowWrap::BreakWord);
  assert_eq!(case.styles.overflow_anchor, OverflowAnchor::None);
  assert_eq!(case.styles.scroll_behavior, ScrollBehavior::Smooth);
  assert_eq!(case.styles.text_align, TextAlign::Center);
  assert_eq!(case.styles.vertical_align, VerticalAlign::Top);
  assert!(case.styles.vertical_align_specified);
  assert_eq!(case.styles.text_indent.length, Length::px(10.0));
  assert!(case.styles.text_indent.hanging);
  assert!(case.styles.text_indent.each_line);
  assert_eq!(case.styles.ruby_position, RubyPosition::Under);
  assert_eq!(case.styles.ruby_align, RubyAlign::Center);
  assert_eq!(case.styles.ruby_merge, RubyMerge::Collapse);

  let lineheight = find_by_id(&styled, "lineheight").expect("lineheight element");
  assert_eq!(lineheight.styles.line_height, LineHeight::Normal);

  let break_el = find_by_id(&styled, "break").expect("break element");
  assert_eq!(break_el.styles.break_before, BreakBetween::Avoid);
  assert_eq!(break_el.styles.break_inside, BreakInside::AvoidColumn);

  let invalid = find_by_id(&styled, "invalid").expect("invalid element");
  assert_eq!(invalid.styles.vertical_align, VerticalAlign::Baseline);
  assert!(!invalid.styles.vertical_align_specified);
}

