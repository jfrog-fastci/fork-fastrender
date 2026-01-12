use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const DEFAULT_FIXTURES_ROOT: &str = "tests/pages/fixtures";
const MAX_NODES_PER_FILE: usize = 500_000;
const MAX_SRCSET_CANDIDATES: usize = 128;
const MAX_JSONISH_BYTES: usize = 16 * 1024;
const DEFAULT_TOP: usize = 25;

#[derive(Args, Debug)]
pub struct AnalyzeLazyLoadingArgs {
  /// Root directory containing offline fixture directories.
  #[arg(long, value_name = "DIR", default_value = DEFAULT_FIXTURES_ROOT)]
  pub fixtures_root: PathBuf,

  /// Only analyze these fixtures (repeatable, e.g. `--fixture apple.com --fixture espn.com`).
  ///
  /// When omitted, analyzes all fixture directories under `--fixtures-root` (excluding the shared
  /// `assets/` directory).
  #[arg(long, value_name = "NAME")]
  pub fixture: Vec<String>,

  /// Scan all `*.html` files under each fixture directory (instead of just `**/index.html`).
  #[arg(long)]
  pub all_html: bool,

  /// Include `*.html` files nested under `assets/` directories.
  ///
  /// By default `assets/` is skipped because fixture subresource bundles occasionally contain
  /// extremely large HTML blobs that aren't representative of the captured DOM.
  #[arg(long, requires = "all_html")]
  pub include_assets_html: bool,

  /// Print the top N `data-*` attribute names (by frequency) per element type.
  #[arg(long, value_name = "N", default_value_t = DEFAULT_TOP)]
  pub top: usize,

  /// Write a full JSON report to this path.
  #[arg(long, value_name = "PATH")]
  pub json: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
struct AnalyzeLazyLoadingReport {
  fixtures_root: String,
  fixtures: Vec<String>,
  html_files: usize,
  elements: BTreeMap<String, ElementStats>,
}

#[derive(Debug, Default, Serialize)]
struct ElementStats {
  total: usize,

  src_missing: usize,
  src_placeholder: usize,
  src_non_placeholder: usize,

  srcset_missing: usize,
  srcset_empty: usize,
  srcset_unparseable: usize,
  srcset_placeholder_only: usize,
  srcset_non_placeholder: usize,

  data_url_attrs: BTreeMap<String, usize>,
}

fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
  value
    .as_bytes()
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn looks_like_url(value: &str) -> bool {
  let value = trim_ascii_whitespace(value);
  if value.is_empty() {
    return false;
  }
  if value.contains("://") || value.starts_with('/') {
    return true;
  }
  if starts_with_ignore_ascii_case(value, "data:") || starts_with_ignore_ascii_case(value, "blob:")
  {
    return true;
  }
  value.contains('.')
}

fn url_from_jsonish(value: &str) -> Option<String> {
  fn extract(value: &serde_json::Value) -> Option<String> {
    match value {
      serde_json::Value::String(s) => {
        let trimmed = trim_ascii_whitespace(s);
        if trimmed.is_empty() || !looks_like_url(trimmed) {
          return None;
        }
        Some(trimmed.to_string())
      }
      serde_json::Value::Array(values) => values.iter().find_map(extract),
      serde_json::Value::Object(map) => {
        const PRIORITY_KEYS: [&str; 7] = [
          "url",
          "src",
          "poster",
          "href",
          "poster_url",
          "posterUrl",
          "imageUrl",
        ];
        for key in PRIORITY_KEYS {
          if let Some(value) = map.get(key) {
            if let Some(url) = extract(value) {
              return Some(url);
            }
          }
        }
        map.values().find_map(extract)
      }
      _ => None,
    }
  }

  let value = trim_ascii_whitespace(value);
  if value.is_empty() || value.len() > MAX_JSONISH_BYTES {
    return None;
  }
  let first = value.chars().next()?;
  if first != '{' && first != '[' && first != '"' {
    return None;
  }
  let parsed = serde_json::from_str::<serde_json::Value>(value).ok()?;
  extract(&parsed)
}

fn discover_fixture_dirs(
  fixtures_root: &Path,
  filter: &[String],
) -> Result<Vec<(String, PathBuf)>> {
  if !filter.is_empty() {
    let mut out = Vec::with_capacity(filter.len());
    for name in filter {
      let dir = fixtures_root.join(name);
      if !dir.is_dir() {
        bail!(
          "fixture directory {} does not exist under {}",
          name,
          fixtures_root.display()
        );
      }
      out.push((name.clone(), dir));
    }
    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    return Ok(out);
  }

  let mut out = Vec::new();
  for entry in fs::read_dir(fixtures_root).with_context(|| {
    format!(
      "failed to read fixtures root directory {}",
      fixtures_root.display()
    )
  })? {
    let entry = entry.context("read_dir entry")?;
    let path = entry.path();
    if !path.is_dir() {
      continue;
    }
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("fixture directory name is not valid UTF-8"))?;
    if name == "assets" {
      continue;
    }
    out.push((name, path));
  }
  out.sort_by(|(a, _), (b, _)| a.cmp(b));
  Ok(out)
}

fn discover_html_files(
  fixture_dir: &Path,
  all_html: bool,
  include_assets_html: bool,
) -> Result<Vec<PathBuf>> {
  let mut files = Vec::new();

  for entry in WalkDir::new(fixture_dir) {
    let entry = entry.context("walkdir entry")?;
    if !entry.file_type().is_file() {
      continue;
    }
    let path = entry.path();

    if !include_assets_html && path.iter().any(|seg| seg == "assets") {
      continue;
    }

    let is_html = path
      .extension()
      .and_then(|s| s.to_str())
      .is_some_and(|ext| ext.eq_ignore_ascii_case("html"));
    if !is_html {
      continue;
    }

    if all_html {
      files.push(path.to_path_buf());
    } else if path
      .file_name()
      .and_then(|s| s.to_str())
      .is_some_and(|name| name.eq_ignore_ascii_case("index.html"))
    {
      files.push(path.to_path_buf());
    }
  }

  files.sort();
  Ok(files)
}

fn element_key(tag: &str) -> Option<&'static str> {
  if tag.eq_ignore_ascii_case("img") {
    Some("img")
  } else if tag.eq_ignore_ascii_case("source") {
    Some("source")
  } else if tag.eq_ignore_ascii_case("iframe") {
    Some("iframe")
  } else if tag.eq_ignore_ascii_case("video") {
    Some("video")
  } else if tag.eq_ignore_ascii_case("audio") {
    Some("audio")
  } else {
    None
  }
}

fn analyze_src(stats: &mut ElementStats, value: Option<&str>) {
  match value {
    None => stats.src_missing += 1,
    Some(value) => {
      if fastrender::dom::img_src_is_placeholder(value) {
        stats.src_placeholder += 1;
      } else {
        stats.src_non_placeholder += 1;
      }
    }
  }
}

fn analyze_srcset(stats: &mut ElementStats, value: Option<&str>) {
  match value {
    None => stats.srcset_missing += 1,
    Some(value) => {
      let trimmed = trim_ascii_whitespace(value);
      if trimmed.is_empty() {
        stats.srcset_empty += 1;
        return;
      }

      let candidates =
        fastrender::html::image_attrs::parse_srcset_with_limit(trimmed, MAX_SRCSET_CANDIDATES);
      if candidates.is_empty() {
        stats.srcset_unparseable += 1;
        return;
      }

      let all_placeholder = candidates
        .iter()
        .all(|candidate| fastrender::dom::img_src_is_placeholder(&candidate.url));
      if all_placeholder {
        stats.srcset_placeholder_only += 1;
      } else {
        stats.srcset_non_placeholder += 1;
      }
    }
  }
}

fn record_data_url_attrs(stats: &mut ElementStats, node: &fastrender::dom::DomNode) {
  for (name, value) in node.attributes_iter() {
    if name.len() < "data-".len() {
      continue;
    }
    if !name.as_bytes()[..5].eq_ignore_ascii_case(b"data-") {
      continue;
    }

    let trimmed = trim_ascii_whitespace(value);
    if trimmed.is_empty() {
      continue;
    }

    let urlish = if looks_like_url(trimmed) {
      true
    } else {
      url_from_jsonish(trimmed).is_some()
    };
    if !urlish {
      continue;
    }

    let key = name.to_ascii_lowercase();
    *stats.data_url_attrs.entry(key).or_insert(0) += 1;
  }
}

fn analyze_dom(root: &fastrender::dom::DomNode, report: &mut AnalyzeLazyLoadingReport) {
  // Avoid recursion and avoid traversing template contents (which are inert and often huge).
  let mut visited = 0usize;
  let mut stack: Vec<&fastrender::dom::DomNode> = vec![root];

  while let Some(node) = stack.pop() {
    visited += 1;
    if visited > MAX_NODES_PER_FILE {
      break;
    }

    if let Some(tag) = node.tag_name() {
      if let Some(key) = element_key(tag) {
        let stats = report
          .elements
          .get_mut(key)
          .expect("elements map should be pre-populated");
        stats.total += 1;

        analyze_src(stats, node.get_attribute_ref("src"));
        if key == "img" || key == "source" {
          analyze_srcset(stats, node.get_attribute_ref("srcset"));
        }
        record_data_url_attrs(stats, node);
      }
    }

    if node.is_template_element() {
      continue;
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
}

fn sorted_top_attrs(attrs: &BTreeMap<String, usize>, top: usize) -> Vec<(String, usize)> {
  let mut pairs: Vec<(String, usize)> = attrs.iter().map(|(k, v)| (k.clone(), *v)).collect();
  pairs.sort_by(|(ka, va), (kb, vb)| vb.cmp(va).then_with(|| ka.cmp(kb)));
  pairs.truncate(top);
  pairs
}

fn fmt_percent(n: usize, d: usize) -> String {
  if d == 0 {
    return "0.0%".to_string();
  }
  format!("{:.1}%", (n as f64) * 100.0 / (d as f64))
}

fn print_element_summary(label: &str, stats: &ElementStats, top: usize) {
  println!("{label}: total={}", stats.total);

  let missing_or_placeholder = stats.src_missing + stats.src_placeholder;
  if missing_or_placeholder > 0 {
    println!(
      "  src missing/placeholder: {missing_or_placeholder} ({}) [missing={}, placeholder={}]",
      fmt_percent(missing_or_placeholder, stats.total),
      stats.src_missing,
      stats.src_placeholder
    );
  }

  let srcset_total = stats.srcset_missing
    + stats.srcset_empty
    + stats.srcset_unparseable
    + stats.srcset_placeholder_only
    + stats.srcset_non_placeholder;
  if srcset_total > 0 {
    println!(
      "  srcset placeholder-only: {} ({}) [missing={}, empty={}, unparseable={}, non-placeholder={}]",
      stats.srcset_placeholder_only,
      fmt_percent(stats.srcset_placeholder_only, srcset_total),
      stats.srcset_missing,
      stats.srcset_empty,
      stats.srcset_unparseable,
      stats.srcset_non_placeholder
    );
  }

  let top_attrs = sorted_top_attrs(&stats.data_url_attrs, top);
  if top_attrs.is_empty() {
    println!("  data-* URL attrs: <none>");
  } else {
    let joined = top_attrs
      .into_iter()
      .map(|(name, count)| format!("{name} ({count})"))
      .collect::<Vec<_>>()
      .join(", ");
    println!("  data-* URL attrs: {joined}");
  }
}

pub fn run_analyze_lazy_loading(args: AnalyzeLazyLoadingArgs) -> Result<()> {
  if args.top == 0 {
    bail!("--top must be >= 1");
  }

  let repo_root = crate::repo_root();
  let fixtures_root = resolve_repo_path(&repo_root, &args.fixtures_root);
  if !fixtures_root.is_dir() {
    bail!(
      "fixtures root directory does not exist: {}",
      fixtures_root.display()
    );
  }

  let fixtures = discover_fixture_dirs(&fixtures_root, &args.fixture)?;
  if fixtures.is_empty() {
    bail!(
      "no fixture directories found under {}",
      fixtures_root.display()
    );
  }

  let mut report = AnalyzeLazyLoadingReport {
    fixtures_root: fixtures_root.display().to_string(),
    fixtures: fixtures.iter().map(|(name, _)| name.clone()).collect(),
    html_files: 0,
    elements: BTreeMap::from([
      ("audio".to_string(), ElementStats::default()),
      ("iframe".to_string(), ElementStats::default()),
      ("img".to_string(), ElementStats::default()),
      ("source".to_string(), ElementStats::default()),
      ("video".to_string(), ElementStats::default()),
    ]),
  };

  for (fixture_name, fixture_dir) in &fixtures {
    let html_files = discover_html_files(fixture_dir, args.all_html, args.include_assets_html)
      .with_context(|| format!("discover HTML files under fixture {fixture_name}"))?;
    for html_path in html_files {
      report.html_files += 1;
      let html =
        fs::read_to_string(&html_path).with_context(|| format!("read {}", html_path.display()))?;
      let dom = fastrender::dom::parse_html(&html)
        .with_context(|| format!("parse DOM from {}", html_path.display()))?;
      analyze_dom(&dom, &mut report);
    }
  }

  println!("analyze-lazy-loading");
  println!("  fixtures_root: {}", report.fixtures_root);
  println!("  fixtures: {}", report.fixtures.join(", "));
  println!("  html files: {}", report.html_files);
  println!();

  // Print in a fixed order to keep the stdout report stable.
  for (key, label) in [
    ("img", "IMG"),
    ("source", "SOURCE"),
    ("iframe", "IFRAME"),
    ("video", "VIDEO"),
    ("audio", "AUDIO"),
  ] {
    let stats = report
      .elements
      .get(key)
      .expect("elements map should contain all keys");
    print_element_summary(label, stats, args.top);
    println!();
  }

  if let Some(json_path_arg) = args.json {
    let json_path = resolve_repo_path(&repo_root, &json_path_arg);
    if let Some(parent) = json_path.parent() {
      if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
      }
    }
    let file =
      fs::File::create(&json_path).with_context(|| format!("create {}", json_path.display()))?;
    serde_json::to_writer_pretty(file, &report).context("serialize JSON report")?;
    println!("Wrote JSON report to {}", json_path.display());
  }

  Ok(())
}
