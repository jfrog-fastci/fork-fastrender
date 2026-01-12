use fastrender::css::parser::parse_stylesheet;
use fastrender::geometry::Size;
use fastrender::style::media::MediaContext;
use fastrender::style::page::{resolve_page_style, PageSide};
use fastrender::style::values::Length;

#[test]
fn page_footnote_rule_sets_max_height() {
  let css = "@page { @footnote { max-height: 50px; } }";
  let sheet = parse_stylesheet(css).expect("parse stylesheet");
  let media = MediaContext::print(800.0, 600.0);
  let collected = sheet.collect_page_rules(&media);
  assert_eq!(collected.len(), 1, "expected one @page rule");

  let resolved = resolve_page_style(
    &collected,
    0,
    None,
    PageSide::Right,
    false,
    Size::new(800.0, 600.0),
    16.0,
    None,
  );

  assert_eq!(resolved.footnote_style.max_height, Some(Length::px(50.0)));
}
