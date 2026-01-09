use fastrender::api::{FastRender, LayoutDocumentOptions};
use fastrender::geometry::Point;
use fastrender::style::media::MediaType;
use fastrender::style::position::Position;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use fastrender::Rgba;

fn find_form_control<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(
    &node.content,
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(_),
      ..
    }
  ) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_form_control(child) {
      return Some(found);
    }
  }
  None
}

fn find_form_control_with_origin<'a>(
  node: &'a FragmentNode,
  origin: Point,
) -> Option<(&'a FragmentNode, Point)> {
  if matches!(
    &node.content,
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(_),
      ..
    }
  ) {
    return Some((node, origin));
  }
  for child in node.children.iter() {
    let child_origin = Point::new(
      origin.x + child.bounds.origin.x,
      origin.y + child.bounds.origin.y,
    );
    if let Some(found) = find_form_control_with_origin(child, child_origin) {
      return Some(found);
    }
  }
  None
}

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

fn layout_html(html: &str) -> FragmentTree {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  renderer
    .layout_document_for_media_with_options(
      &dom,
      240,
      180,
      MediaType::Screen,
      LayoutDocumentOptions::new(),
      None,
    )
    .expect("layout")
}

#[test]
fn form_control_out_of_flow_pseudo_elements_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .search-input {
            position: relative;
            width: 120px;
            height: 32px;
            padding-left: 20px;
            border: 1px solid black;
          }
          .search-input::before {
            content: "";
            position: absolute;
            left: 4px;
            top: 4px;
            width: 10px;
            height: 10px;
            background: rgb(123, 45, 67);
            display: block;
          }
        </style>
      </head>
      <body>
        <input class="search-input" value="">
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(123, 45, 67);
  let fragments = layout_html(html);

  let input_fragment =
    find_form_control(&fragments.root).expect("expected a form control fragment");
  let pseudo_fragment = find_fragment_by_background(input_fragment, target_color)
    .expect("expected pseudo-element fragment inside form control");

  assert!(
    pseudo_fragment.bounds.width() > 0.0 && pseudo_fragment.bounds.height() > 0.0,
    "expected pseudo-element to have non-zero size; got {:?}",
    pseudo_fragment.bounds
  );
  assert!(
    pseudo_fragment
      .style
      .as_ref()
      .is_some_and(|style| matches!(style.position, Position::Absolute)),
    "expected pseudo-element fragment to be positioned"
  );
}

#[test]
fn form_control_absolute_pseudo_uses_nearest_positioned_ancestor() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; padding-top: 50px; }
          input {
            display: block;
            margin-top: 40px;
            margin-left: 100px;
            width: 120px;
            height: 32px;
            padding: 0;
            border: 0;
          }
          input::before {
            content: "";
            position: absolute;
            left: 10px;
            top: 20px;
            width: 10px;
            height: 10px;
            background: rgb(123, 45, 67);
            display: block;
          }
        </style>
      </head>
      <body>
        <input value="">
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(123, 45, 67);
  let fragments = layout_html(html);
  let (input_fragment, input_origin) = find_form_control_with_origin(&fragments.root, Point::ZERO)
    .expect("expected a form control fragment");
  let pseudo_fragment = find_fragment_by_background(input_fragment, target_color)
    .expect("expected pseudo-element fragment inside form control");

  // The input is `position: static`, so the absolutely positioned pseudo-element should not use the
  // input as its containing block. With no positioned ancestor, it should be positioned against
  // the initial containing block (viewport), meaning its offset inside the input is negative when
  // the input itself is placed away from the origin.
  assert!(
    pseudo_fragment
      .style
      .as_ref()
      .is_some_and(|style| matches!(style.position, Position::Absolute)),
    "expected pseudo-element fragment to be absolutely positioned"
  );
  assert!(
    pseudo_fragment.bounds.x() < 0.0 && pseudo_fragment.bounds.y() < 0.0,
    "expected abspos pseudo-element to not use the input as its containing block; input at {:?}; got {:?}",
    input_origin,
    pseudo_fragment.bounds
  );
}

#[test]
fn form_control_fixed_pseudo_is_viewport_fixed_in_block_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; padding-top: 50px; }
          input {
            display: block;
            margin-left: 100px;
            width: 120px;
            height: 32px;
            padding: 0;
            border: 0;
          }
          input::before {
            content: "";
            position: fixed;
            left: 10px;
            top: 20px;
            width: 10px;
            height: 10px;
            background: rgb(123, 45, 67);
            display: block;
          }
        </style>
      </head>
      <body>
        <input value="">
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(123, 45, 67);
  let fragments = layout_html(html);
  let input_fragment =
    find_form_control(&fragments.root).expect("expected a form control fragment");
  let pseudo_fragment = find_fragment_by_background(input_fragment, target_color)
    .expect("expected pseudo-element fragment inside form control");

  assert!(
    pseudo_fragment
      .style
      .as_ref()
      .is_some_and(|style| matches!(style.position, Position::Fixed)),
    "expected pseudo-element fragment to be fixed-positioned"
  );
  assert!(
    (pseudo_fragment.bounds.x() - 10.0).abs() < 0.5
      && (pseudo_fragment.bounds.y() - 20.0).abs() < 0.5,
    "expected viewport-fixed pseudo-element to be at (10, 20); got {:?}",
    pseudo_fragment.bounds
  );
}

#[test]
fn form_control_fixed_pseudo_is_viewport_fixed_in_inline_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          p { margin: 0; padding-top: 50px; padding-left: 100px; }
          input {
            width: 120px;
            height: 32px;
            padding: 0;
            border: 0;
          }
          input::before {
            content: "";
            position: fixed;
            left: 10px;
            top: 20px;
            width: 10px;
            height: 10px;
            background: rgb(123, 45, 67);
            display: block;
          }
        </style>
      </head>
      <body>
        <p><input value=""></p>
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(123, 45, 67);
  let fragments = layout_html(html);
  let input_fragment =
    find_form_control(&fragments.root).expect("expected a form control fragment");
  let pseudo_fragment = find_fragment_by_background(input_fragment, target_color)
    .expect("expected pseudo-element fragment inside form control");

  assert!(
    pseudo_fragment
      .style
      .as_ref()
      .is_some_and(|style| matches!(style.position, Position::Fixed)),
    "expected pseudo-element fragment to be fixed-positioned"
  );
  assert!(
    (pseudo_fragment.bounds.x() - 10.0).abs() < 0.5
      && (pseudo_fragment.bounds.y() - 20.0).abs() < 0.5,
    "expected viewport-fixed pseudo-element to be at (10, 20); got {:?}",
    pseudo_fragment.bounds
  );
}

#[test]
fn form_control_fixed_pseudo_is_viewport_fixed_in_flex_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .row { display: flex; padding-top: 50px; padding-left: 100px; }
          input {
            width: 120px;
            height: 32px;
            padding: 0;
            border: 0;
          }
          input::before {
            content: "";
            position: fixed;
            left: 10px;
            top: 20px;
            width: 10px;
            height: 10px;
            background: rgb(123, 45, 67);
            display: block;
          }
        </style>
      </head>
      <body>
        <div class="row"><input value=""></div>
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(123, 45, 67);
  let fragments = layout_html(html);
  let input_fragment =
    find_form_control(&fragments.root).expect("expected a form control fragment");
  let pseudo_fragment = find_fragment_by_background(input_fragment, target_color)
    .expect("expected pseudo-element fragment inside form control");

  assert!(
    pseudo_fragment
      .style
      .as_ref()
      .is_some_and(|style| matches!(style.position, Position::Fixed)),
    "expected pseudo-element fragment to be fixed-positioned"
  );
  assert!(
    (pseudo_fragment.bounds.x() - 10.0).abs() < 0.5
      && (pseudo_fragment.bounds.y() - 20.0).abs() < 0.5,
    "expected viewport-fixed pseudo-element to be at (10, 20); got {:?}",
    pseudo_fragment.bounds
  );
}
