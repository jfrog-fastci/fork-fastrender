use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
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
fn max_length_function_survives_var_resolution() {
  let css = r#"
    :root {
      --pad-min: 1rem;
      --pad: max(var(--pad-min), calc(50vw - 720px + 1rem));
    }

    #t {
      padding-inline: var(--pad);
    }
  "#;

  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();

  let resolve = |vw: f32| {
    let media = MediaContext::screen(vw, 800.0);
    let styled = apply_styles_with_media(&dom, &stylesheet, &media);
    let target = find_by_id(&styled, "t").expect("element with id t");
    target
      .styles
      .padding_left
      .resolve_with_context(
        None,
        vw,
        800.0,
        target.styles.font_size,
        target.styles.root_font_size,
      )
      .expect("resolved padding-left")
  };

  // 50vw - 720px + 1rem is negative at 1040px, so max() should pick 1rem.
  assert!((resolve(1040.0) - 16.0).abs() < 1e-3);

  // At larger viewports the calc() branch becomes positive and max() should pick it.
  assert!((resolve(2000.0) - 296.0).abs() < 1e-3);
}

#[test]
fn clamp_length_function_survives_var_resolution() {
  let css = r#"
    :root {
      --val: clamp(1rem, calc(50vw - 720px + 1rem), 10rem);
    }

    #t {
      margin-left: var(--val);
    }
  "#;

  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();

  let resolve = |vw: f32| {
    let media = MediaContext::screen(vw, 800.0);
    let styled = apply_styles_with_media(&dom, &stylesheet, &media);
    let target = find_by_id(&styled, "t").expect("element with id t");
    let margin_left = target.styles.margin_left.expect("margin-left");
    margin_left
      .resolve_with_context(
        None,
        vw,
        800.0,
        target.styles.font_size,
        target.styles.root_font_size,
      )
      .expect("resolved margin-left")
  };

  // Preferred branch is negative at 1040px; clamp() should pick the minimum 1rem.
  assert!((resolve(1040.0) - 16.0).abs() < 1e-3);

  // Preferred branch is larger than the upper bound at 2000px; clamp() should pick 10rem.
  assert!((resolve(2000.0) - 160.0).abs() < 1e-3);
}

