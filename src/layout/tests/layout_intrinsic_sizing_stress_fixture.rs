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

fn find_probe_intrinsic<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. }) {
    let mut chips = Vec::new();
    for child in node.children.iter() {
      if matches!(child.content, FragmentContent::Block { .. }) {
        chips.push(child);
      }
    }

    if chips.len() == 10
      && chips.iter().all(|chip| {
        (chip.bounds.width() - 20.0).abs() < 0.5 && (chip.bounds.height() - 10.0).abs() < 0.5
      })
    {
      return Some(node);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_probe_intrinsic(child) {
      return Some(found);
    }
  }

  None
}

#[test]
fn layout_intrinsic_sizing_stress_fixture_has_expected_probe_geometry() {
  let html_path = fixture_path("layout_intrinsic_sizing_stress");
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

  let probe = find_probe_intrinsic(&fragments.root).expect("probe flex container should exist");

  // Probe width is determined by 10 fixed-size chips + flex gaps/padding. Use a tolerant range so
  // minor rounding changes don't break the guardrail.
  assert!(
    probe.bounds.width() > 210.0 && probe.bounds.width() < 230.0,
    "expected probe width near 220px, got {:.3}",
    probe.bounds.width()
  );
  assert!(
    probe.bounds.height() > 10.0 && probe.bounds.height() < 20.0,
    "expected probe height near 12px, got {:.3}",
    probe.bounds.height()
  );

  // Ensure the fixture isn't "dead" (stress subtree should contribute a non-trivial fragment count).
  let node_count = fragments.root.node_count();
  assert!(
    node_count > 1500,
    "expected stress fixture to produce many fragments (got {node_count})"
  );
}
