use fastrender::api::FastRender;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

fn collect_form_control_bounds(fragment: &FragmentNode, out: &mut Vec<fastrender::Rect>) {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if matches!(
      &node.content,
      FragmentContent::Replaced {
        replaced_type: ReplacedType::FormControl(_),
        ..
      }
    ) {
      out.push(node.bounds);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
}

#[test]
fn textarea_field_sizing_content_increases_intrinsic_height() {
  let html = "<style>textarea { padding: 0; border: 0; }</style>\
    <div><textarea>one\ntwo\nthree</textarea></div>\
    <div><textarea style=\"field-sizing: content\">one\ntwo\nthree</textarea></div>";

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("dom");
  let tree = renderer.layout_document(&dom, 400, 200).expect("layout");

  let mut bounds = Vec::new();
  collect_form_control_bounds(&tree.root, &mut bounds);
  assert_eq!(bounds.len(), 2, "expected exactly two textareas");

  let fixed_height = bounds[0].height();
  let content_height = bounds[1].height();
  assert!(
    content_height > fixed_height + 0.5,
    "expected field-sizing: content textarea to be taller: fixed={fixed_height} content={content_height}",
  );
}

#[test]
fn input_field_sizing_content_shrinks_intrinsic_width() {
  let html = "<style>input { padding: 0; border: 0; }</style>\
    <div><input value=\"0\"></div>\
    <div><input style=\"field-sizing: content\" value=\"0\"></div>";

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("dom");
  let tree = renderer.layout_document(&dom, 400, 200).expect("layout");

  let mut bounds = Vec::new();
  collect_form_control_bounds(&tree.root, &mut bounds);
  assert_eq!(bounds.len(), 2, "expected exactly two inputs");

  let fixed_width = bounds[0].width();
  let content_width = bounds[1].width();
  assert!(
    content_width < fixed_width - 1.0,
    "expected field-sizing: content input to be narrower: fixed={fixed_width} content={content_width}",
  );
}

#[test]
fn field_sizing_content_respects_min_max_width() {
  let html = "<style>input { padding: 0; border: 0; }</style>\
    <div><input style=\"field-sizing: content; min-width: 100px\" value=\"0\"></div>\
    <div><input style=\"field-sizing: content; max-width: 50px\" value=\"000000000000000000000000000000\"></div>";

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("dom");
  let tree = renderer.layout_document(&dom, 400, 200).expect("layout");

  let mut bounds = Vec::new();
  collect_form_control_bounds(&tree.root, &mut bounds);
  assert_eq!(bounds.len(), 2, "expected exactly two inputs");

  let min_width = bounds[0].width();
  assert!(
    (min_width - 100.0).abs() <= 0.5,
    "expected min-width to clamp field-sizing width: got {min_width}",
  );

  let max_width = bounds[1].width();
  assert!(
    (max_width - 50.0).abs() <= 0.5,
    "expected max-width to clamp field-sizing width: got {max_width}",
  );
}

