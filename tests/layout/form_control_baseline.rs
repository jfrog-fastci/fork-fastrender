use fastrender::style::display::Display;
use fastrender::style::types::Appearance;
use fastrender::text::font_db::FontConfig;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::box_tree::{
  FormControl, FormControlKind, ReplacedType, SelectControl, SelectItem, TextControlKind,
};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, BoxTree, ComputedStyle, FormattingContextType, LayoutConfig, LayoutEngine, Size};
use std::sync::Arc;

fn find_first_line<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. }) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_line(child) {
      return Some(found);
    }
  }
  None
}

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

fn baseline_and_control_bottom(
  kind: FormControlKind,
  intrinsic_size: Size,
) -> (f32, f32) {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut inline_style = root_style.clone();
  inline_style.font_size = 16.0;

  let text_style = Arc::new(inline_style.clone());
  let control_style = Arc::new(inline_style.clone());

  let text = BoxNode::new_text(text_style, "X".to_string());
  let control = BoxNode::new_replaced(
    control_style,
    ReplacedType::FormControl(FormControl {
      control: kind,
      appearance: Appearance::Auto,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
    }),
    Some(intrinsic_size),
    None,
  );

  let inline_fc = BoxNode::new_block(
    Arc::new(inline_style),
    FormattingContextType::Inline,
    vec![text, control],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![inline_fc],
  );
  let tree = BoxTree::new(root);

  let config = LayoutConfig::for_viewport(Size::new(400.0, 200.0));
  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let engine = LayoutEngine::with_font_context(config, font_context);
  let fragments = engine.layout_tree(&tree).expect("layout tree");

  let line_fragment = find_first_line(&fragments.root).expect("expected a line fragment");
  let FragmentContent::Line { baseline } = line_fragment.content else {
    unreachable!();
  };
  let control_fragment =
    find_form_control(line_fragment).expect("expected form control fragment on the first line");

  (baseline, control_fragment.bounds.max_y())
}

fn baseline_and_line_height_for_text_only() -> (f32, f32) {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut inline_style = root_style.clone();
  inline_style.font_size = 16.0;

  let text_style = Arc::new(inline_style.clone());

  let text = BoxNode::new_text(text_style, "X".to_string());

  let inline_fc = BoxNode::new_block(
    Arc::new(inline_style),
    FormattingContextType::Inline,
    vec![text],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![inline_fc],
  );
  let tree = BoxTree::new(root);

  let config = LayoutConfig::for_viewport(Size::new(400.0, 200.0));
  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let engine = LayoutEngine::with_font_context(config, font_context);
  let fragments = engine.layout_tree(&tree).expect("layout tree");

  let line_fragment = find_first_line(&fragments.root).expect("expected a line fragment");
  let FragmentContent::Line { baseline } = line_fragment.content else {
    unreachable!();
  };

  (baseline, line_fragment.bounds.height())
}

#[test]
fn inline_text_like_form_controls_use_text_baseline() {
  let epsilon = 0.01;
  // Fragment bounds are expressed in their containing fragment's coordinate space; compare within
  // the line fragment. Old behavior used the replaced baseline (bottom edge), which would make
  // these equal.

  let intrinsic = Size::new(100.0, 40.0);
  let cases = [
    (
      "input[type=text]",
      FormControlKind::Text {
        value: String::new(),
        placeholder: None,
        placeholder_style: None,
        size_attr: None,
        kind: TextControlKind::Plain,
      },
    ),
    (
      "textarea",
      FormControlKind::TextArea {
        value: String::new(),
        placeholder: None,
        placeholder_style: None,
        rows: None,
        cols: None,
      },
    ),
    (
      "select",
      FormControlKind::Select(SelectControl {
        multiple: false,
        size: 1,
        items: Arc::new(vec![SelectItem::Option {
          node_id: 1,
          label: "Option".to_string(),
          value: "option".to_string(),
          selected: true,
          disabled: false,
          in_optgroup: false,
        }]),
        selected: vec![0],
      }),
    ),
    ("button", FormControlKind::Button { label: "Ok".to_string() }),
  ];

  for (label, kind) in cases {
    let (baseline_y, control_bottom_y) = baseline_and_control_bottom(kind, intrinsic);
    assert!(
      control_bottom_y > baseline_y + epsilon,
      "expected {label} control to extend below the line baseline: bottom={control_bottom_y:.3} baseline={baseline_y:.3}",
    );
  }
}

#[test]
fn inline_non_text_form_control_keeps_replaced_baseline() {
  // Non-text controls keep the default replaced baseline (bottom edge). That means the control's
  // bottom edge coincides with the line baseline.
  let (baseline_y, control_bottom_y) = baseline_and_control_bottom(
    FormControlKind::Checkbox {
      is_radio: false,
      checked: false,
      indeterminate: false,
    },
    Size::new(16.0, 16.0),
  );
  let epsilon = 0.01;
  assert!(
    (control_bottom_y - baseline_y).abs() <= epsilon,
    "expected checkbox baseline to be bottom edge: bottom={control_bottom_y:.3} baseline={baseline_y:.3}",
  );
}

#[test]
fn inline_unknown_form_control_without_label_keeps_replaced_baseline() {
  // Unknown controls without a label don't paint text, so they should keep the default replaced
  // baseline (bottom edge).
  let (baseline_y, control_bottom_y) = baseline_and_control_bottom(
    FormControlKind::Unknown { label: None },
    Size::new(16.0, 16.0),
  );
  let epsilon = 0.01;
  assert!(
    (control_bottom_y - baseline_y).abs() <= epsilon,
    "expected unknown-without-label baseline to be bottom edge: bottom={control_bottom_y:.3} baseline={baseline_y:.3}",
  );
}

#[test]
fn inline_baseline_accounts_for_centered_form_control_text() {
  let epsilon = 0.01;
  let (text_baseline, line_height) = baseline_and_line_height_for_text_only();
  assert!(line_height.is_finite() && line_height > 0.0);

  // Use a content-box height larger than the line-height so the control's native painting centers
  // its internal text vertically.
  let content_height = line_height + 20.0;
  let intrinsic = Size::new(100.0, content_height);
  let vertical_offset = ((content_height - line_height) / 2.0).max(0.0);
  let expected = text_baseline + vertical_offset;

  let centered_cases = [
    (
      "input[type=text]",
      FormControlKind::Text {
        value: String::new(),
        placeholder: None,
        placeholder_style: None,
        size_attr: None,
        kind: TextControlKind::Plain,
      },
    ),
    ("input-button", FormControlKind::Button { label: "Ok".to_string() }),
    (
      "unknown-with-label",
      FormControlKind::Unknown {
        label: Some("Unknown".to_string()),
      },
    ),
  ];

  for (label, kind) in centered_cases {
    let (baseline_y, _) = baseline_and_control_bottom(kind, intrinsic);
    assert!(
      (baseline_y - expected).abs() <= epsilon,
      "expected {label} baseline to include vertical centering offset: got={baseline_y:.3} expected={expected:.3}",
    );
  }

  // Controls that do not center their text in native painting should keep the baseline at the
  // content's line-height position.
  let non_centered_cases = [(
    "textarea",
    FormControlKind::TextArea {
      value: String::new(),
      placeholder: None,
      placeholder_style: None,
      rows: None,
      cols: None,
    },
  )];

  for (label, kind) in non_centered_cases {
    let (baseline_y, _) = baseline_and_control_bottom(kind, intrinsic);
    assert!(
      (baseline_y - text_baseline).abs() <= epsilon,
      "expected {label} baseline to ignore vertical centering offset: got={baseline_y:.3} expected={text_baseline:.3}",
    );
  }
}
