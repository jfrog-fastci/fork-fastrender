use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles;
use crate::style::cascade::StyledNode;
use crate::style::types::BorderCollapse;
use crate::style::types::CaptionSide;
use crate::style::types::EmptyCells;
use crate::style::types::FontKerning;
use crate::style::types::FontSynthesis;
use crate::style::types::FontWeight;
use crate::style::types::TableLayout;
use crate::style::types::TextWrap;

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
fn font_and_table_keywords_are_ascii_case_insensitive() {
  let dom = dom::parse_html(
    r#"
      <div id="font"></div>
      <div id="table"></div>
      <div id="text"></div>
    "#,
  )
  .expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      #font {
        font-weight: BOLD;
        font-kerning: NONE;
        font-synthesis: WEIGHT STYLE;
      }

      #table {
        table-layout: FIXED;
        empty-cells: HIDE;
        caption-side: BOTTOM;
        border-collapse: COLLAPSE;
      }

      #text { text-wrap: PRETTY; }
    "#,
  )
  .expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let font = find_by_id(&styled, "font").expect("font element");
  assert_eq!(font.styles.font_weight, FontWeight::Bold);
  assert_eq!(font.styles.font_kerning, FontKerning::None);
  assert_eq!(
    font.styles.font_synthesis,
    FontSynthesis {
      weight: true,
      style: true,
      small_caps: false,
      position: false,
    }
  );

  let table = find_by_id(&styled, "table").expect("table element");
  assert_eq!(table.styles.table_layout, TableLayout::Fixed);
  assert_eq!(table.styles.empty_cells, EmptyCells::Hide);
  assert_eq!(table.styles.caption_side, CaptionSide::Bottom);
  assert_eq!(table.styles.border_collapse, BorderCollapse::Collapse);

  let text = find_by_id(&styled, "text").expect("text element");
  assert_eq!(text.styles.text_wrap, TextWrap::Pretty);
}
