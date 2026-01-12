//! Guardrail to ensure conformance targets are documented and enforced.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

#[test]
fn conformance_doc_is_present_and_non_empty() {
  let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/conformance.md");
  assert!(
    conformance.exists(),
    "docs/conformance.md should exist as the conformance source of truth"
  );

  let content = std::fs::read_to_string(&conformance).expect("read docs/conformance.md");
  assert!(
    !content.trim().is_empty(),
    "docs/conformance.md should not be empty"
  );
}

#[test]
fn conformance_doc_links_to_real_code_and_tests() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let conformance_path = root.join("docs/conformance.md");
  let content = std::fs::read_to_string(&conformance_path).expect("read docs/conformance.md");

  // Keep links in docs/conformance.md grounded in real modules and tests for each feature area.
  let required_paths = [
    "src/dom.rs",
    "src/html/mod.rs",
    "src/html/encoding.rs",
    "src/html/viewport.rs",
    "src/css/parser.rs",
    "src/css/selectors.rs",
    "src/css/types.rs",
    "src/style/cascade.rs",
    "src/style/media.rs",
    "src/style/values.rs",
    "src/style/color.rs",
    "src/tree/box_generation.rs",
    "src/tree/box_tree.rs",
    "src/tree/table_fixup.rs",
    "src/layout/table.rs",
    "src/layout/contexts/block/mod.rs",
    "src/layout/contexts/inline/mod.rs",
    "src/layout/absolute_positioning.rs",
    "src/layout/contexts/flex.rs",
    "src/layout/contexts/grid.rs",
    "src/layout/taffy_integration.rs",
    "src/layout/fragmentation.rs",
    "src/layout/pagination.rs",
    "src/scroll.rs",
    "src/paint/stacking.rs",
    "src/paint/display_list.rs",
    "src/paint/clip_path.rs",
    "src/paint/display_list_renderer.rs",
    "src/paint/svg_filter.rs",
    "src/paint/text_rasterize.rs",
    "src/paint/text_shadow.rs",
    "src/text/pipeline.rs",
    "src/text/line_break.rs",
    "src/text/hyphenation.rs",
    "src/text/justify.rs",
    "src/animation/mod.rs",
    "src/accessibility.rs",
    "src/js/legacy/ecma_embed.rs",
    "src/js/vmjs/window.rs",
    "src/js/vmjs/window_realm.rs",
    "src/js/event_loop.rs",
    "src/js/html_script_processing.rs",
    "src/js/webidl/mod.rs",
    "src/js/legacy/vm_dom.rs",
    "src/js/vmjs/window_timers.rs",
    "src/js/url.rs",
    "src/js/vmjs/window_url.rs",
    "src/js/fetch.rs",
    "src/js/vmjs/window_fetch.rs",
    "src/js/legacy/quickjs/fetch.rs",
    "tests/dom_integration/compatibility_test.rs",
    "tests/tree/shadow_dom.rs",
    "tests/css_integration/loader_tests.rs",
    "tests/style/has_selector_test.rs",
    "tests/style/layer_important_test.rs",
    "tests/style/media_test.rs",
    "tests/style/supports_rule_test.rs",
    "tests/style/css_numeric_functions.rs",
    "tests/paint/color_mix_display_list_test.rs",
    "tests/tree/test_anonymous_boxes.rs",
    "tests/tree/form_option_nonrendered.rs",
    "tests/layout/table_columns_test.rs",
    "tests/layout/test_inline_float.rs",
    "tests/layout/test_positioned.rs",
    "tests/layout/flex_box_sizing_test.rs",
    "tests/layout/subgrid.rs",
    "tests/layout/table_anonymous_inheritance.rs",
    "tests/layout/multicol.rs",
    "tests/layout/paged_media.rs",
    "tests/layout/scrollbar_gutter.rs",
    "tests/paint/stacking_test.rs",
    "tests/paint/display_list_test.rs",
    "tests/paint/display_list_renderer_test.rs",
    "tests/paint/text_rasterize_test.rs",
    "tests/paint/display_list_font_palette_overrides_test.rs",
    "tests/text/pipeline_test.rs",
    "tests/text/line_break_test.rs",
    "tests/text/hyphenation_test.rs",
    "tests/text/justify_test.rs",
    "tests/animation/mod.rs",
    "tests/bin/fetch_and_render_animation_time_test.rs",
    "tests/accessibility/test.rs",
    "tests/accessibility/name_computation.rs",
    "tests/misc/integration_test.rs",
    "tests/style/container_style_queries.rs",
    "tests/html_script_processing.rs",
    "tests/bin/fetch_and_render_js_test.rs",
  ];

  for path in required_paths {
    assert!(
      content.contains(path),
      "docs/conformance.md should mention {path} so the matrix stays tied to the code/tests"
    );
    assert!(
      root.join(path).exists(),
      "Documented path {path} should exist relative to the repo root"
    );
  }

  // Validate that the support matrix table is structurally parseable:
  // - header exists
  // - every row has 6 columns
  // - status column uses the legend markers
  let mut in_table = false;
  for (idx, line) in content.lines().enumerate() {
    let trimmed = line.trim();
    if !in_table {
      if trimmed.starts_with("| Stage") {
        in_table = true;
      } else {
        continue;
      }
    }

    if !trimmed.starts_with('|') {
      break;
    }

    let parts: Vec<&str> = trimmed.split('|').collect();
    // A well-formed markdown row looks like: | a | b | ... |.
    // That yields an empty first/last element.
    assert!(
      parts.len() >= 3,
      "docs/conformance.md support matrix row is malformed at line {}: {trimmed:?}",
      idx + 1
    );
    let cols = parts.len() - 2;
    assert_eq!(
      cols, 6,
      "docs/conformance.md support matrix row must have 6 columns (found {cols}) at line {}: {trimmed:?}",
      idx + 1
    );

    // Skip header + delimiter rows.
    if trimmed.starts_with("| Stage") || trimmed.starts_with("| ---") {
      continue;
    }

    let status = parts[3].trim(); // Stage | Feature | Status | ...
    assert!(
      matches!(status, "✅" | "⚠️" | "🚫"),
      "docs/conformance.md support matrix status must be ✅/⚠️/🚫 (got {status:?}) at line {}",
      idx + 1
    );
  }
  assert!(
    in_table,
    "docs/conformance.md should contain a support matrix table starting with a `| Stage` header row"
  );

  // Validate that every markdown link target resolves to an existing path (relative to docs/).
  // This guards against doc drift when files are renamed/moved.
  let link_re =
    regex::Regex::new(r"\[[^\]]*]\(([^)]+)\)").expect("regex for markdown links should compile");
  let mut linked: HashSet<String> = HashSet::new();
  for cap in link_re.captures_iter(&content) {
    let raw_target = cap.get(1).expect("link target capture").as_str().trim();

    // Support the common Markdown forms:
    //   [text](path)
    //   [text](path#fragment)
    //   [text](path "title")
    // We intentionally keep this lightweight (not a full Markdown parser).
    let raw_target = raw_target
      .split_whitespace()
      .next()
      .unwrap_or_default()
      .trim_matches('<')
      .trim_matches('>');
    let raw_target = raw_target
      .split_once('#')
      .map(|(path, _frag)| path)
      .unwrap_or(raw_target);
    let raw_target = raw_target
      .split_once('?')
      .map(|(path, _query)| path)
      .unwrap_or(raw_target);

    if raw_target.is_empty()
      || raw_target.starts_with('#')
      || raw_target.starts_with("http://")
      || raw_target.starts_with("https://")
      || raw_target.starts_with("mailto:")
    {
      continue;
    }

    linked.insert(raw_target.to_string());
  }

  assert!(
    linked.iter().any(|p| p.starts_with("../src/")),
    "docs/conformance.md should link to at least one source file under ../src/"
  );
  assert!(
    linked.iter().any(|p| p.starts_with("../tests/")),
    "docs/conformance.md should link to at least one test file under ../tests/"
  );

  let docs_dir = conformance_path
    .parent()
    .expect("docs/conformance.md should have a parent directory");
  let mut missing = Vec::<(String, PathBuf)>::new();
  for path in linked {
    let resolved = docs_dir.join(&path);
    if !resolved.exists() {
      missing.push((path, resolved));
    }
  }

  if !missing.is_empty() {
    missing.sort_by(|a, b| a.0.cmp(&b.0));
    let formatted = missing
      .into_iter()
      .map(|(rel, abs)| format!("{rel} (resolved to {})", abs.display()))
      .collect::<Vec<_>>()
      .join("\n");
    panic!("docs/conformance.md contains links to paths that do not exist:\n{formatted}");
  }
}
