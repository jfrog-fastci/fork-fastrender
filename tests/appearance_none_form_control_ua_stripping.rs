use fastrender::{FastRender, FragmentNode, Rgba};

fn find_fragment_by_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, color) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_by_text_color<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.color == color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_text_color(child, color) {
      return Some(found);
    }
  }
  None
}

#[test]
fn appearance_none_form_controls_strip_ua_border_padding_background_color() {
  // UA default `border/padding/background-color` for form controls must not leak into layout/paint
  // when `appearance: none` disables native replaced control rendering (Chromium behavior).
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #cb { appearance: none; width: 20px; height: 10px; background: rgb(1, 2, 3); }
          #text { appearance: none; width: 120px; height: 30px; background: rgb(4, 5, 6); }
          #ta { appearance: none; width: 150px; height: 40px; background: rgb(7, 8, 9); }
          #cb_border {
            appearance: none;
            width: 20px;
            height: 10px;
            border: 5px solid red;
            background: rgb(10, 11, 12);
          }
          /* No author background; should become transparent (UA background stripped). */
          #cb_ua_bg { appearance: none; width: 17px; height: 19px; color: rgb(13, 14, 15); }
        </style>
      </head>
      <body>
        <input id="cb" type="checkbox">
        <input id="text" type="text">
        <textarea id="ta"></textarea>
        <input id="cb_border" type="checkbox">
        <input id="cb_ua_bg" type="checkbox">
      </body>
    </html>"#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 400, 200).expect("layout");

  let eps = 1e-3;

  let cb_color = Rgba::rgb(1, 2, 3);
  let cb_fragment =
    find_fragment_by_background(&fragments.root, cb_color).expect("checkbox fragment");
  assert_eq!(
    cb_fragment.style.as_ref().unwrap().background_color,
    cb_color,
    "author background should be preserved"
  );
  assert!(
    (cb_fragment.bounds.width() - 20.0).abs() < eps
      && (cb_fragment.bounds.height() - 10.0).abs() < eps,
    "expected checkbox bounds to equal authored size; got {:?}",
    cb_fragment.bounds
  );

  let text_color = Rgba::rgb(4, 5, 6);
  let text_fragment =
    find_fragment_by_background(&fragments.root, text_color).expect("text input fragment");
  assert_eq!(
    text_fragment.style.as_ref().unwrap().background_color,
    text_color,
    "author background should be preserved"
  );
  assert!(
    (text_fragment.bounds.width() - 120.0).abs() < eps
      && (text_fragment.bounds.height() - 30.0).abs() < eps,
    "expected text input bounds to equal authored size; got {:?}",
    text_fragment.bounds
  );

  let ta_color = Rgba::rgb(7, 8, 9);
  let ta_fragment =
    find_fragment_by_background(&fragments.root, ta_color).expect("textarea fragment");
  assert_eq!(
    ta_fragment.style.as_ref().unwrap().background_color,
    ta_color,
    "author background should be preserved"
  );
  assert!(
    (ta_fragment.bounds.width() - 150.0).abs() < eps
      && (ta_fragment.bounds.height() - 40.0).abs() < eps,
    "expected textarea bounds to equal authored size; got {:?}",
    ta_fragment.bounds
  );

  // Author border must be preserved under `appearance:none`.
  let bordered_color = Rgba::rgb(10, 11, 12);
  let bordered_fragment = find_fragment_by_background(&fragments.root, bordered_color)
    .expect("bordered checkbox fragment");
  assert_eq!(
    bordered_fragment.style.as_ref().unwrap().background_color,
    bordered_color,
    "author background should be preserved"
  );
  assert!(
    (bordered_fragment.bounds.width() - 30.0).abs() < eps
      && (bordered_fragment.bounds.height() - 20.0).abs() < eps,
    "expected author border to inflate bounds; got {:?}",
    bordered_fragment.bounds
  );

  // UA background-color should be stripped when it is the winning (UA) value.
  let ua_text_color = Rgba::rgb(13, 14, 15);
  let ua_fragment =
    find_fragment_by_text_color(&fragments.root, ua_text_color).expect("UA-bg checkbox fragment");
  assert!(
    (ua_fragment.bounds.width() - 17.0).abs() < eps
      && (ua_fragment.bounds.height() - 19.0).abs() < eps,
    "expected UA-bg checkbox bounds to equal authored size; got {:?}",
    ua_fragment.bounds
  );
  assert_eq!(
    ua_fragment.style.as_ref().unwrap().background_color,
    Rgba::TRANSPARENT,
    "expected UA background-color to be stripped to transparent"
  );
}
