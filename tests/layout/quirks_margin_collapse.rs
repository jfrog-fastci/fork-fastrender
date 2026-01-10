use fastrender::debug::runtime::RuntimeToggles;
use fastrender::layout::engine::LayoutParallelism;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use fastrender::{FastRender, FastRenderConfig, RenderArtifactRequest, RenderArtifacts, RenderOptions};
use std::collections::HashMap;

const EPS: f32 = 0.1;

fn render_tree(html: &str, width: u32, height: u32) -> FragmentTree {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("create renderer");

  let options = RenderOptions::new().with_viewport(width, height);
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    fragment_tree: true,
    ..Default::default()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render");

  artifacts.fragment_tree.expect("fragment tree artifact")
}

fn block_y_for_text(fragment: &FragmentNode, needle: &str) -> Option<f32> {
  fn walk(node: &FragmentNode, abs_y: f32, blocks: &mut Vec<f32>, needle: &str) -> Option<f32> {
    let abs_y = abs_y + node.bounds.y();
    let pushed = matches!(node.content, FragmentContent::Block { .. });
    if pushed {
      blocks.push(abs_y);
    }

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return blocks.last().copied();
      }
    }

    for child in node.children.iter() {
      if let Some(found) = walk(child, abs_y, blocks, needle) {
        return Some(found);
      }
    }

    if pushed {
      blocks.pop();
    }
    None
  }

  let mut blocks = Vec::new();
  walk(fragment, 0.0, &mut blocks, needle)
}

#[test]
fn quirks_mode_discards_user_agent_default_margins_at_top_of_document() {
  // No doctype -> quirks mode.
  let html = r#"
    <style>body { margin: 0; }</style>
    <p>Hello</p>
  "#;

  let tree = render_tree(html, 200, 200);
  let y = block_y_for_text(&tree.root, "Hello").expect("text fragment");
  assert!(
    (y - 0.0).abs() <= EPS,
    "expected UA default top margins to be ignored in quirks mode (got y={y})"
  );
}

#[test]
fn quirks_mode_preserves_author_margins_at_top_of_document() {
  let html = r#"
    <style>
      body { margin: 0; }
      p { margin-top: 16px; }
    </style>
    <p>Hello</p>
  "#;

  let tree = render_tree(html, 200, 200);
  let y = block_y_for_text(&tree.root, "Hello").expect("text fragment");
  assert!(
    (y - 16.0).abs() <= EPS,
    "expected authored margins to push the body down in quirks mode (got y={y})"
  );
}

#[test]
fn quirks_mode_ignores_user_agent_margins_recursively_when_collapsing() {
  let html = r#"
    <style>
      body { margin: 0; }
      #outer { margin-top: 5px; }
    </style>
    <div id="outer">
      <p>Hello</p>
    </div>
  "#;

  let tree = render_tree(html, 200, 200);
  let y = block_y_for_text(&tree.root, "Hello").expect("text fragment");
  assert!(
    (y - 5.0).abs() <= EPS,
    "expected UA margins inside the collapsing chain to be ignored (got y={y})"
  );
}

