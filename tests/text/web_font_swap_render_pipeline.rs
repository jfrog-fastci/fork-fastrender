use fastrender::api::{FastRender, FastRenderConfig, RenderArtifactRequest, RenderOptions};
use fastrender::text::font_db::FontConfig;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;
use url::Url;

fn collect_text_run_fonts(node: &FragmentNode, out: &mut Vec<String>) {
  if let FragmentContent::Text { shaped, .. } = &node.content {
    if let Some(runs) = shaped {
      for run in runs.iter() {
        out.push(run.font.family.clone());
      }
    }
  }
  for child in node.children.iter() {
    collect_text_run_fonts(child, out);
  }
}

#[test]
fn swap_web_fonts_are_used_by_render_pipeline() {
  let dir = tempdir().expect("tempdir");
  let mut font_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  font_path.push("tests/fixtures/fonts/NotoSans-subset.ttf");
  let bytes = match fs::read(&font_path) {
    Ok(bytes) => bytes,
    Err(_) => return,
  };

  fs::write(dir.path().join("swap.ttf"), bytes).expect("write font");

  let base_url =
    Url::from_file_path(dir.path().join("index.html")).expect("file url").to_string();

  let html = r#"
<!doctype html>
<html>
  <head>
    <style>
      @font-face {
        font-family: 'SwapFont';
        font-style: normal;
        font-weight: 400;
        font-display: swap;
        src: url('swap.ttf') format('truetype');
      }
      body { font-family: 'SwapFont', sans-serif; font-size: 16px; }
    </style>
  </head>
  <body>Aa</body>
</html>
"#;

  let config = FastRenderConfig::new().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let options = RenderOptions::new().with_viewport(200, 100);
  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      &base_url,
      options,
      RenderArtifactRequest {
        fragment_tree: true,
        ..Default::default()
      },
    )
    .expect("render");

  let tree = report
    .artifacts
    .fragment_tree
    .as_ref()
    .expect("expected fragment tree artifact");

  let mut fonts = Vec::new();
  collect_text_run_fonts(&tree.root, &mut fonts);
  assert!(
    fonts.iter().any(|family| family == "SwapFont"),
    "expected a shaped run using the swap web font; got {fonts:?}"
  );
}
