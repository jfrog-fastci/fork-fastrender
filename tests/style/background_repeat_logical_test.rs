use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::BackgroundRepeatKeyword;

fn collect_divs<'a>(node: &'a StyledNode, out: &mut Vec<&'a StyledNode>) {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case("div") {
      out.push(node);
    }
  }
  for child in node.children.iter() {
    collect_divs(child, out);
  }
}

fn all_divs(styled: &StyledNode) -> Vec<&StyledNode> {
  let mut out = Vec::new();
  collect_divs(styled, &mut out);
  out
}

fn repeat_xy(node: &StyledNode) -> (BackgroundRepeatKeyword, BackgroundRepeatKeyword) {
  let rep = node
    .styles
    .background_repeats
    .first()
    .copied()
    .expect("background repeat");
  (rep.x, rep.y)
}

#[test]
fn background_repeat_x_and_y_longhands_set_single_axis() {
  let dom = dom::parse_html(
    r#"<div style="background-repeat-x: no-repeat"></div>
       <div style="background-repeat-y: no-repeat"></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let divs = all_divs(&styled);

  assert_eq!(
    repeat_xy(divs[0]),
    (BackgroundRepeatKeyword::NoRepeat, BackgroundRepeatKeyword::Repeat)
  );
  assert_eq!(
    repeat_xy(divs[1]),
    (BackgroundRepeatKeyword::Repeat, BackgroundRepeatKeyword::NoRepeat)
  );
}

#[test]
fn background_repeat_inline_respects_final_writing_mode_regardless_of_declaration_order() {
  let dom = dom::parse_html(
    r#"<div style="writing-mode: vertical-rl; background-repeat-inline: no-repeat"></div>
       <div style="background-repeat-inline: no-repeat; writing-mode: vertical-rl"></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let divs = all_divs(&styled);

  // In vertical-rl, the inline axis maps to the physical y-axis, so `background-repeat-inline`
  // sets `y` and leaves `x` at its initial `repeat` value.
  for div in divs {
    assert_eq!(
      repeat_xy(div),
      (BackgroundRepeatKeyword::Repeat, BackgroundRepeatKeyword::NoRepeat)
    );
  }
}

#[test]
fn background_repeat_repeat_inline_and_block_keywords_map_logical_axes() {
  let dom = dom::parse_html(
    r#"<div style="background-repeat: repeat-inline"></div>
       <div style="background-repeat: repeat-block"></div>
       <div style="writing-mode: vertical-rl; background-repeat: repeat-inline"></div>
       <div style="background-repeat: repeat-inline; writing-mode: vertical-rl"></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let divs = all_divs(&styled);

  // Default writing mode is horizontal-tb, so repeat-inline == repeat-x and repeat-block == repeat-y.
  assert_eq!(
    repeat_xy(divs[0]),
    (BackgroundRepeatKeyword::Repeat, BackgroundRepeatKeyword::NoRepeat)
  );
  assert_eq!(
    repeat_xy(divs[1]),
    (BackgroundRepeatKeyword::NoRepeat, BackgroundRepeatKeyword::Repeat)
  );

  // In vertical-rl, inline maps to the physical y-axis, so repeat-inline == repeat-y.
  assert_eq!(
    repeat_xy(divs[2]),
    (BackgroundRepeatKeyword::NoRepeat, BackgroundRepeatKeyword::Repeat)
  );
  assert_eq!(
    repeat_xy(divs[3]),
    (BackgroundRepeatKeyword::NoRepeat, BackgroundRepeatKeyword::Repeat)
  );
}

