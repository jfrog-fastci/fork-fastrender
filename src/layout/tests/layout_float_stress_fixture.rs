use crate::geometry::Rect;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig, ResourcePolicy};
use std::fs;
use std::path::PathBuf;
use url::Url;

fn fixture_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures")
    .join(name)
    .join("index.html")
}

fn base_url_for(html_path: &PathBuf) -> String {
  let dir = html_path
    .parent()
    .unwrap_or_else(|| panic!("fixture html has no parent dir: {}", html_path.display()));
  Url::from_directory_path(dir)
    .unwrap_or_else(|_| panic!("failed to build file:// base URL for {}", dir.display()))
    .to_string()
}

fn find_block_with_line_children_and_width<'a>(
  node: &'a FragmentNode,
  target_width: f32,
  expected_lines: usize,
) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && (node.bounds.width() - target_width).abs() < 0.5
    && node
      .children
      .iter()
      .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
      .count()
      == expected_lines
  {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) =
      find_block_with_line_children_and_width(child, target_width, expected_lines)
    {
      return Some(found);
    }
  }

  None
}

fn line_bounds_for_probe(root: &FragmentNode) -> Vec<Rect> {
  let block = find_block_with_line_children_and_width(root, 100.0, 3)
    .expect("expected probe block fragment with exactly three line children");
  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| line.bounds)
    .collect()
}

#[test]
fn layout_float_stress_fixture_has_expected_probe_line_geometry() {
  let html_path = fixture_path("layout_float_stress");
  let html =
    fs::read_to_string(&html_path).unwrap_or_else(|e| panic!("read {}: {e}", html_path.display()));
  let base_url = base_url_for(&html_path);

  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);
  let mut renderer = FastRender::builder()
    .base_url(base_url)
    .font_sources(FontConfig::bundled_only())
    .resource_policy(policy)
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(&html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 1040, 1240)
    .expect("layout fixture");

  let lines = line_bounds_for_probe(&fragments.root);
  assert_eq!(lines.len(), 3);

  // The first two lines overlap the left float's vertical span (0-20), so they are shifted right
  // and narrowed. The third line begins at y=20 and should have full width.
  assert!((lines[0].x() - 60.0).abs() < 0.5);
  assert!((lines[0].y() - 0.0).abs() < 0.5);
  assert!((lines[0].width() - 40.0).abs() < 0.5);

  assert!((lines[1].x() - 60.0).abs() < 0.5);
  assert!((lines[1].y() - 10.0).abs() < 0.5);
  assert!((lines[1].width() - 40.0).abs() < 0.5);

  assert!((lines[2].x() - 0.0).abs() < 0.5);
  assert!((lines[2].y() - 20.0).abs() < 0.5);
  assert!((lines[2].width() - 100.0).abs() < 0.5);

  // Ensure the stress float stack isn't "dead".
  let node_count = fragments.root.node_count();
  assert!(
    node_count > 500,
    "expected float stress fixture to produce many fragments (got {node_count})"
  );
}
