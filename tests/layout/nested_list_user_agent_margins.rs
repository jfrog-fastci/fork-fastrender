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
fn nested_lists_have_zero_vertical_user_agent_margins() {
  // Chrome/Firefox UA styles remove top/bottom margins for nested lists (e.g. `ul ul { margin: 0 }`)
  // to avoid excessive whitespace. Without that rule, pages like sqlite.org render visibly taller
  // sidebar lists.
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; font-size: 10px; line-height: 10px; }
    </style>
    <ul>
      <li>ParentText
        <ul><li>ChildText</li></ul>
      </li>
      <li>NextText</li>
    </ul>
  "#;

  let tree = render_tree(html, 200, 200);

  let parent_y = block_y_for_text(&tree.root, "ParentText").expect("ParentText");
  let child_y = block_y_for_text(&tree.root, "ChildText").expect("ChildText");
  let next_y = block_y_for_text(&tree.root, "NextText").expect("NextText");

  assert!(
    (child_y - (parent_y + 10.0)).abs() <= EPS,
    "expected nested <ul> to have no extra top margin (parent_y={parent_y} child_y={child_y})",
  );
  assert!(
    (next_y - (parent_y + 20.0)).abs() <= EPS,
    "expected nested <ul> to have no extra bottom margin (parent_y={parent_y} next_y={next_y})",
  );
}

