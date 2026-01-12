use crate::common::global_state::global_test_lock;
use crate::r#ref::image_compare::{compare_config_from_env, compare_pngs, CompareEnvVars};
use fastrender::api::{FastRender, RenderOptions};
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::FontConfig;
use std::fs;
use std::path::PathBuf;

#[test]
fn visual_fixture_matches_goldens() {
  let _lock = global_test_lock();

  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let html = fs::read_to_string(root.join("tests/fixtures/html/transition_starting_style.html"))
    .expect("fixture html");
  let compare_config = compare_config_from_env(CompareEnvVars::fixtures()).expect("compare config");
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let prepared = renderer
    .prepare_html(&html, RenderOptions::new().with_viewport(260, 180))
    .expect("prepare");
  let cases = [
    ("transition_starting_style_0ms", 0.0),
    ("transition_starting_style_400ms", 400.0),
  ];
  for (name, time) in cases {
    let pixmap = prepared.paint_at_time(time).expect("render");
    let png = encode_image(&pixmap, OutputFormat::Png).expect("encode png");
    let golden_path = root
      .join("tests/fixtures/golden")
      .join(format!("{name}.png"));

    if std::env::var("UPDATE_GOLDEN").is_ok() {
      fs::create_dir_all(golden_path.parent().unwrap()).expect("golden dir");
      fs::write(&golden_path, &png).expect("write golden");
      continue;
    }

    let golden = fs::read(&golden_path).expect("golden png");
    let diff_dir = root.join("target/transition_starting_style_diffs");
    compare_pngs(name, &png, &golden, &compare_config, &diff_dir).expect("compare");
  }
}
