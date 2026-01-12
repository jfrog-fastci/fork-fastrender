use crate::{
  FastRender, FragmentContent, FragmentNode, FragmentTree, LayoutParallelism, RenderOptions,
  ResourcePolicy,
};
use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Once;

static SET_BUNDLED_FONTS: Once = Once::new();

fn ensure_bundled_fonts() {
  SET_BUNDLED_FONTS.call_once(|| {
    // Keep determinism tests consistent across machines by using the bundled font set.
    std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
  });
}

fn create_test_renderer() -> FastRender {
  ensure_bundled_fonts();
  FastRender::builder()
    .resource_policy(ResourcePolicy::default().allow_http(false).allow_https(false))
    .build()
    .expect("renderer")
}

fn read_fixture(rel_path: &str) -> String {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_path);
  std::fs::read_to_string(&path)
    .unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()))
}

fn canon_f32(value: f32) -> i64 {
  // Layout coordinates should never be NaN/inf; when they are, preserve the signal in the diff.
  if !value.is_finite() {
    if value.is_nan() {
      return i64::MIN;
    }
    if value.is_sign_negative() {
      return i64::MIN + 1;
    }
    return i64::MAX;
  }
  // Treat values within ~1/1000 CSS px as equivalent for fingerprinting. This keeps the output
  // stable while still surfacing any layout deltas large enough to affect painting.
  ((value as f64) * 1000.0).round() as i64
}

fn format_canon(value: f32) -> String {
  if !value.is_finite() {
    return format!("{value}");
  }
  let canon = canon_f32(value) as f64 / 1000.0;
  format!("{canon:.3}")
}

fn summarize_text(text: &str) -> String {
  const LIMIT: usize = 32;
  let mut out = String::new();
  for ch in text.chars().take(LIMIT) {
    match ch {
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      other => out.push(other),
    }
  }
  if text.chars().count() > LIMIT {
    out.push('…');
  }
  out
}

fn content_fingerprint(content: &FragmentContent) -> String {
  match content {
    FragmentContent::Block { box_id } => format!("block box_id={:?}", box_id),
    FragmentContent::Inline {
      box_id,
      fragment_index,
    } => format!("inline box_id={:?} inline_idx={fragment_index}", box_id),
    FragmentContent::Text {
      text,
      box_id,
      baseline_offset,
      is_marker,
      ..
    } => format!(
      "text box_id={:?} baseline_offset={} marker={} len={} \"{}\"",
      box_id,
      format_canon(*baseline_offset),
      is_marker,
      text.len(),
      summarize_text(text.as_ref())
    ),
    FragmentContent::Line { baseline } => format!("line baseline={}", format_canon(*baseline)),
    FragmentContent::Replaced {
      replaced_type,
      box_id,
    } => format!("replaced box_id={:?} {replaced_type:?}", box_id),
    FragmentContent::RunningAnchor { name, .. } => format!("running_anchor \"{name}\""),
    FragmentContent::FootnoteAnchor { .. } => "footnote_anchor".to_string(),
  }
}

#[derive(Debug, Clone)]
struct FragmentFingerprint {
  hash: u64,
  lines: Vec<String>,
}

fn fingerprint_tree(tree: &FragmentTree) -> FragmentFingerprint {
  let mut lines = Vec::new();
  let viewport = tree.viewport_size();
  lines.push(format!(
    "viewport {}x{} roots={}",
    format_canon(viewport.width),
    format_canon(viewport.height),
    1 + tree.additional_fragments.len()
  ));

  fn walk(root_label: &str, node: &FragmentNode, path: &mut Vec<usize>, out: &mut Vec<String>) {
    let mut path_str = String::from(root_label);
    for idx in path.iter() {
      path_str.push('/');
      path_str.push_str(&idx.to_string());
    }

    out.push(format!(
      "{path_str} {} bounds=({}, {}, {}, {}) overflow=({}, {}, {}, {}) frag={}/{} fragainer_idx={} fragainer=({},{:?},{:?}) children={}",
      content_fingerprint(&node.content),
      format_canon(node.bounds.x()),
      format_canon(node.bounds.y()),
      format_canon(node.bounds.width()),
      format_canon(node.bounds.height()),
      format_canon(node.scroll_overflow.x()),
      format_canon(node.scroll_overflow.y()),
      format_canon(node.scroll_overflow.width()),
      format_canon(node.scroll_overflow.height()),
      node.fragment_index,
      node.fragment_count,
      node.fragmentainer_index,
      node.fragmentainer.page_index,
      node.fragmentainer.column_set_index,
      node.fragmentainer.column_index,
      node.children.len(),
    ));

    for (idx, child) in node.children.iter().enumerate() {
      path.push(idx);
      walk(root_label, child, path, out);
      path.pop();
    }
  }

  let mut path = Vec::new();
  walk("root[0]", &tree.root, &mut path, &mut lines);
  for (idx, extra) in tree.additional_fragments.iter().enumerate() {
    path.clear();
    walk(&format!("root[{}]", idx + 1), extra, &mut path, &mut lines);
  }

  let mut hasher = DefaultHasher::new();
  for line in &lines {
    line.hash(&mut hasher);
  }
  FragmentFingerprint {
    hash: hasher.finish(),
    lines,
  }
}

fn diff_fingerprints(
  case: &str,
  a_label: &str,
  a: &FragmentFingerprint,
  b_label: &str,
  b: &FragmentFingerprint,
) -> String {
  let mut idx = 0usize;
  while idx < a.lines.len() && idx < b.lines.len() && a.lines[idx] == b.lines[idx] {
    idx += 1;
  }

  let mut out = String::new();
  out.push_str(&format!(
    "fragment fingerprint mismatch for {case}\n{a_label} hash={:016x} lines={}\n{b_label} hash={:016x} lines={}\n",
    a.hash,
    a.lines.len(),
    b.hash,
    b.lines.len()
  ));

  if idx == a.lines.len() && idx == b.lines.len() {
    out.push_str("note: hashes differed but line streams were identical (unexpected)\n");
    return out;
  }

  out.push_str(&format!("first mismatch at line {idx}\n"));
  out.push_str(&format!(
    "{a_label}: {}\n",
    a.lines.get(idx).map(String::as_str).unwrap_or("<eof>")
  ));
  out.push_str(&format!(
    "{b_label}: {}\n",
    b.lines.get(idx).map(String::as_str).unwrap_or("<eof>")
  ));

  let start = idx.saturating_sub(3);
  let end = (idx + 4).max(start).min(a.lines.len().max(b.lines.len()));
  out.push_str("\ncontext:\n");
  for i in start..end {
    out.push_str(&format!(
      "  {i:04} | {a_label}: {}\n",
      a.lines.get(i).map(String::as_str).unwrap_or("<eof>")
    ));
    out.push_str(&format!(
      "       | {b_label}: {}\n",
      b.lines.get(i).map(String::as_str).unwrap_or("<eof>")
    ));
  }

  out
}

fn assert_fingerprint_eq(case: &str, a_label: &str, a: FragmentFingerprint, b_label: &str, b: FragmentFingerprint) {
  if a.lines != b.lines {
    panic!("{}", diff_fingerprints(case, a_label, &a, b_label, &b));
  }
}

fn assert_layout_deterministic(case: &str, html: &str, viewport: (u32, u32)) {
  let serial_fingerprint = {
    let mut renderer = create_test_renderer();
    let options = RenderOptions::new()
      .with_viewport(viewport.0, viewport.1)
      .with_layout_parallelism(LayoutParallelism::disabled());
    let doc = renderer.prepare_html(html, options).expect("serial prepare");
    fingerprint_tree(doc.fragment_tree())
  };

  let parallel_fingerprint = {
    let mut renderer = create_test_renderer();
    let parallelism = LayoutParallelism::enabled(1).with_max_threads(Some(2));
    let options = RenderOptions::new()
      .with_viewport(viewport.0, viewport.1)
      .with_layout_parallelism(parallelism);
    let doc = renderer.prepare_html(html, options).expect("parallel prepare");
    fingerprint_tree(doc.fragment_tree())
  };

  assert_fingerprint_eq(case, "serial", serial_fingerprint, "parallel", parallel_fingerprint);
}

#[test]
fn layout_parallelism_produces_deterministic_fragment_fingerprints() {
  static LOCK: Mutex<()> = Mutex::new(());
  let _guard = LOCK.lock();

  let fixtures = [
    (
      "wpt/layout/nested-floats-001.html",
      "tests/wpt/tests/layout/nested-floats-001.html",
      (220, 180),
    ),
    (
      "wpt/layout/floats/line-box-both-floats-push-down-001.html",
      "tests/wpt/tests/layout/floats/line-box-both-floats-push-down-001.html",
      (220, 180),
    ),
    (
      "wpt/layout/flex-row-001.html",
      "tests/wpt/tests/layout/flex-row-001.html",
      (360, 140),
    ),
    (
      "wpt/layout/grid-percent-tracks-001.html",
      "tests/wpt/tests/layout/grid-percent-tracks-001.html",
      (240, 160),
    ),
  ];

  for (case, path, viewport) in fixtures {
    let html = read_fixture(path);
    assert_layout_deterministic(case, &html, viewport);
  }

  let synthetic_inline = concat!(
    "<!doctype html>",
    "<meta charset=utf-8>",
    "<style>",
    "body{margin:0;font-family:\"Noto Sans\",sans-serif;font-size:16px;}",
    ".wrap{width:220px;padding:6px;border:1px solid #000;}",
    ".measure{display:inline-block;width:max-content;letter-spacing:0.25px;}",
    ".em{font-size:20px;font-weight:600;}",
    "</style>",
    "<div class=wrap>",
    "<span class=measure>",
    "<span class=em>Intrinsics:</span> ",
    "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor.",
    "</span>",
    "</div>",
  );
  assert_layout_deterministic("synthetic/inline-intrinsic-text", synthetic_inline, (260, 120));
}
