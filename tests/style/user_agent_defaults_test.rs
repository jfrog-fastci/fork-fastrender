use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles, StyledNode};
use fastrender::style::display::Display;
use fastrender::style::types::{FontStyle, TextAlign, VerticalAlign, WhiteSpace};
use fastrender::style::values::Length;

fn find_styled_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }

  node
    .children
    .iter()
    .find_map(|child| find_styled_by_id(child, id))
}

fn styled_tree(html: &str) -> StyledNode {
  let dom = dom::parse_html(html).expect("parse html");
  apply_styles(&dom, &StyleSheet::new())
}

fn assert_length_px(len: &Length, expected: f32) {
  assert_eq!(
    *len,
    Length::px(expected),
    "expected {expected}px, got {len:?}"
  );
}

#[test]
fn user_agent_body_margin_and_heading_defaults() {
  let styled = styled_tree(
    r#"
      <!doctype html>
      <html>
        <body id="body">
          <strong id="strong_outer">
            Outer <strong id="strong_inner">Inner</strong>
          </strong>
          <strong>
            <h1 id="h1">Heading 1</h1>
          </strong>
          <h2 id="h2">Heading 2</h2>
          <address id="address">Address</address>
        </body>
      </html>
    "#,
  );

  let body = find_styled_by_id(&styled, "body").expect("body styled");
  assert_eq!(body.styles.display, Display::Block);
  assert_eq!(body.styles.margin_top, Some(Length::px(8.0)));
  assert_eq!(body.styles.margin_right, Some(Length::px(8.0)));
  assert_eq!(body.styles.margin_bottom, Some(Length::px(8.0)));
  assert_eq!(body.styles.margin_left, Some(Length::px(8.0)));

  let h1 = find_styled_by_id(&styled, "h1").expect("h1 styled");
  assert_eq!(h1.styles.display, Display::Block);
  assert!((h1.styles.font_size - 32.0).abs() < 1e-6, "h1 font-size");
  assert_eq!(h1.styles.font_weight.to_u16(), 700, "h1 is bold (not bolder)");

  let h2 = find_styled_by_id(&styled, "h2").expect("h2 styled");
  assert_eq!(h2.styles.display, Display::Block);
  assert!((h2.styles.font_size - 24.0).abs() < 1e-6, "h2 font-size");

  let strong_outer = find_styled_by_id(&styled, "strong_outer").expect("strong_outer styled");
  assert_eq!(strong_outer.styles.font_weight.to_u16(), 700, "strong is bolder");
  let strong_inner = find_styled_by_id(&styled, "strong_inner").expect("strong_inner styled");
  assert_eq!(
    strong_inner.styles.font_weight.to_u16(),
    900,
    "nested <strong> should be bolder than its parent"
  );

  let address = find_styled_by_id(&styled, "address").expect("address styled");
  assert_eq!(address.styles.display, Display::Block);
  assert_eq!(address.styles.font_style, FontStyle::Italic);
}

#[test]
fn user_agent_lists_tables_and_pre_defaults() {
  let styled = styled_tree(
    r#"
      <!doctype html>
      <html>
        <body>
          <pre id="pre">x</pre>
          <ul id="ul"><li id="li">Item</li></ul>
          <table id="table">
            <tr id="tr">
              <th id="th">H</th>
              <td id="td">D</td>
            </tr>
          </table>
        </body>
      </html>
    "#,
  );

  let pre = find_styled_by_id(&styled, "pre").expect("pre styled");
  assert_eq!(pre.styles.display, Display::Block);
  assert_eq!(pre.styles.white_space, WhiteSpace::Pre);
  assert_eq!(pre.styles.font_family.first().map(|s| s.as_str()), Some("monospace"));

  let ul = find_styled_by_id(&styled, "ul").expect("ul styled");
  assert_eq!(ul.styles.display, Display::Block);
  assert_length_px(&ul.styles.padding_left, 40.0);

  let li = find_styled_by_id(&styled, "li").expect("li styled");
  assert_eq!(li.styles.display, Display::ListItem);

  let table = find_styled_by_id(&styled, "table").expect("table styled");
  assert_eq!(table.styles.display, Display::Table);
  assert_length_px(&table.styles.border_spacing_horizontal, 2.0);
  assert_length_px(&table.styles.border_spacing_vertical, 2.0);

  let th = find_styled_by_id(&styled, "th").expect("th styled");
  assert_eq!(th.styles.display, Display::TableCell);
  assert_eq!(th.styles.font_weight.to_u16(), 700);
  assert_eq!(th.styles.text_align, TextAlign::Center);

  let td = find_styled_by_id(&styled, "td").expect("td styled");
  assert_eq!(td.styles.display, Display::TableCell);
  assert_length_px(&td.styles.padding_top, 1.0);
  assert_length_px(&td.styles.padding_right, 1.0);
  assert_length_px(&td.styles.padding_bottom, 1.0);
  assert_length_px(&td.styles.padding_left, 1.0);
  assert_eq!(td.styles.vertical_align, VerticalAlign::Middle);
}

#[test]
fn user_agent_details_dialog_popover_and_slot_defaults() {
  let styled = styled_tree(
    r#"
      <!doctype html>
      <html>
        <body>
          <details id="details">
            <summary id="summary">Summary</summary>
            <div id="details_content">Content</div>
          </details>
          <details id="details_open" open>
            <summary id="summary_open">Summary</summary>
            <div id="details_content_open">Content</div>
          </details>

          <dialog id="dialog">Dialog</dialog>
          <dialog id="dialog_open" open>Dialog</dialog>

          <div id="popover" popover>Popover</div>
          <div id="popover_open" popover open>Popover</div>

          <slot id="slot"><span>fallback</span></slot>
        </body>
      </html>
    "#,
  );

  let details = find_styled_by_id(&styled, "details").expect("details styled");
  assert_eq!(details.styles.display, Display::Block);

  let summary = find_styled_by_id(&styled, "summary").expect("summary styled");
  assert_eq!(summary.styles.display, Display::ListItem);

  let details_content = find_styled_by_id(&styled, "details_content").expect("details content styled");
  assert_eq!(details_content.styles.display, Display::None);

  let details_content_open =
    find_styled_by_id(&styled, "details_content_open").expect("details open content styled");
  assert_ne!(details_content_open.styles.display, Display::None);

  let dialog = find_styled_by_id(&styled, "dialog").expect("dialog styled");
  assert_eq!(dialog.styles.display, Display::None);

  let dialog_open = find_styled_by_id(&styled, "dialog_open").expect("dialog_open styled");
  assert_eq!(dialog_open.styles.display, Display::Block);

  let popover = find_styled_by_id(&styled, "popover").expect("popover styled");
  assert_eq!(popover.styles.display, Display::None);

  let popover_open = find_styled_by_id(&styled, "popover_open").expect("popover_open styled");
  assert_eq!(popover_open.styles.display, Display::Block);

  let slot = find_styled_by_id(&styled, "slot").expect("slot styled");
  assert_eq!(slot.styles.display, Display::Contents);
}
