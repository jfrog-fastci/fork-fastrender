use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn collect_form_control_fragments<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if let FragmentContent::Replaced { replaced_type, .. } = &node.content {
    if matches!(replaced_type, ReplacedType::FormControl(_)) {
      out.push(node);
    }
  }
  for child in node.children.iter() {
    collect_form_control_fragments(child, out);
  }
}

#[test]
fn progress_and_meter_intrinsic_sizes_scale_with_font_size() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; }
      progress, meter { border: 0; padding: 0; background: transparent; }
      .row { display: block; }
    </style>
    <div class="row"><progress value="0.5" max="1" style="font-size: 10px"></progress></div>
    <div class="row"><progress value="0.5" max="1" style="font-size: 20px"></progress></div>
    <div class="row"><meter value="0.5" min="0" max="1" style="font-size: 10px"></meter></div>
    <div class="row"><meter value="0.5" min="0" max="1" style="font-size: 20px"></meter></div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let fragments = renderer.layout_document(&dom, 800, 200).expect("layout");

  let mut controls = Vec::new();
  collect_form_control_fragments(&fragments.root, &mut controls);

  let mut progress_bounds = Vec::new();
  let mut meter_bounds = Vec::new();
  for node in controls {
    let FragmentContent::Replaced { replaced_type, .. } = &node.content else {
      continue;
    };
    let ReplacedType::FormControl(control) = replaced_type else {
      continue;
    };
    match &control.control {
      FormControlKind::Progress { .. } => progress_bounds.push(node.bounds),
      FormControlKind::Meter { .. } => meter_bounds.push(node.bounds),
      _ => {}
    }
  }

  assert_eq!(progress_bounds.len(), 2, "expected two progress fragments");
  assert_eq!(meter_bounds.len(), 2, "expected two meter fragments");

  let epsilon = 0.05;
  let progress_10 = progress_bounds[0];
  let progress_20 = progress_bounds[1];
  assert!(
    (progress_10.width() - 100.0).abs() < epsilon && (progress_10.height() - 10.0).abs() < epsilon,
    "expected 10px progress to be ~100x10, got {}x{}",
    progress_10.width(),
    progress_10.height()
  );
  assert!(
    (progress_20.width() - 200.0).abs() < epsilon && (progress_20.height() - 20.0).abs() < epsilon,
    "expected 20px progress to be ~200x20, got {}x{}",
    progress_20.width(),
    progress_20.height()
  );

  let meter_10 = meter_bounds[0];
  let meter_20 = meter_bounds[1];
  assert!(
    (meter_10.width() - 100.0).abs() < epsilon && (meter_10.height() - 10.0).abs() < epsilon,
    "expected 10px meter to be ~100x10, got {}x{}",
    meter_10.width(),
    meter_10.height()
  );
  assert!(
    (meter_20.width() - 200.0).abs() < epsilon && (meter_20.height() - 20.0).abs() < epsilon,
    "expected 20px meter to be ~200x20, got {}x{}",
    meter_20.width(),
    meter_20.height()
  );
}

#[test]
fn progress_and_meter_respect_explicit_size_with_border_box() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; }
      progress, meter { display: block; }
      #p {
        box-sizing: border-box;
        width: 120px;
        height: 20px;
        border: 5px solid black;
        padding: 3px;
      }
      #m {
        box-sizing: border-box;
        width: 200px;
        height: 40px;
        border: 2px solid black;
        padding: 4px;
      }
    </style>
    <progress id="p" value="0.5" max="1"></progress>
    <meter id="m" value="0.5" min="0" max="1"></meter>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let fragments = renderer.layout_document(&dom, 800, 200).expect("layout");

  let mut controls = Vec::new();
  collect_form_control_fragments(&fragments.root, &mut controls);
  assert_eq!(controls.len(), 2, "expected two form control fragments");

  let mut progress = None;
  let mut meter = None;
  for node in controls {
    let FragmentContent::Replaced { replaced_type, .. } = &node.content else {
      continue;
    };
    let ReplacedType::FormControl(control) = replaced_type else {
      continue;
    };
    match &control.control {
      FormControlKind::Progress { .. } => progress = Some(node),
      FormControlKind::Meter { .. } => meter = Some(node),
      _ => {}
    }
  }
  let progress = progress.expect("expected a progress fragment");
  let meter = meter.expect("expected a meter fragment");

  let epsilon = 0.05;
  assert!(
    (progress.bounds.width() - 120.0).abs() < epsilon && (progress.bounds.height() - 20.0).abs() < epsilon,
    "expected border-box progress to be 120x20, got {}x{}",
    progress.bounds.width(),
    progress.bounds.height()
  );
  assert!(
    (meter.bounds.width() - 200.0).abs() < epsilon && (meter.bounds.height() - 40.0).abs() < epsilon,
    "expected border-box meter to be 200x40, got {}x{}",
    meter.bounds.width(),
    meter.bounds.height()
  );
}
