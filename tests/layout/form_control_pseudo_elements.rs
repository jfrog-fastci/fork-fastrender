use fastrender::api::{FastRender, LayoutDocumentOptions};
use fastrender::style::media::MediaType;
use fastrender::style::position::Position;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
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

fn find_fragment_by_background<'a>(node: &'a FragmentNode, color: Rgba) -> Option<&'a FragmentNode> {
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
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document_for_media_with_options(
      &dom,
      240,
      120,
      MediaType::Screen,
      LayoutDocumentOptions::new(),
      None,
    )
    .expect("layout");

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

