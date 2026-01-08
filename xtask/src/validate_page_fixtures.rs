use anyhow::{bail, Context, Result};
use clap::Args;
use regex::Regex;
use std::borrow::Cow;
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use url::Url;
use walkdir::WalkDir;

const DEFAULT_FIXTURES_ROOT: &str = "tests/pages/fixtures";

#[derive(Args, Debug)]
pub struct ValidatePageFixturesArgs {
  /// Root directory containing offline fixtures.
  #[arg(long, default_value = DEFAULT_FIXTURES_ROOT)]
  pub fixtures_root: PathBuf,

  /// Also validate script-related subresources (e.g. remote `<script src>`, modulepreload).
  ///
  /// This is intentionally opt-in because fixtures are commonly rendered in JS-disabled mode, and
  /// scripts are not fetched there.
  #[arg(long)]
  pub include_scripts: bool,

  /// Only validate the listed fixtures (comma-separated).
  #[arg(long, value_delimiter = ',')]
  pub only: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Violation {
  fixture: String,
  file: PathBuf,
  line: usize,
  column: usize,
  url: String,
}

#[derive(Debug, Clone)]
struct UrlSpan {
  url: String,
  start: usize,
  end: usize,
}

struct LineIndex {
  line_starts: Vec<usize>,
}

impl LineIndex {
  fn new(content: &str) -> Self {
    let mut line_starts = Vec::new();
    line_starts.push(0);
    for (idx, b) in content.as_bytes().iter().enumerate() {
      if *b == b'\n' {
        line_starts.push(idx + 1);
      }
    }
    Self { line_starts }
  }

  fn line_col(&self, offset: usize) -> (usize, usize) {
    let idx = match self.line_starts.binary_search(&offset) {
      Ok(pos) => pos,
      Err(pos) => pos.saturating_sub(1),
    };
    let line_start = *self.line_starts.get(idx).unwrap_or(&0);
    (idx + 1, offset.saturating_sub(line_start) + 1)
  }
}

pub fn run_validate_page_fixtures(args: ValidatePageFixturesArgs) -> Result<()> {
  let fixtures_root = if args.fixtures_root.is_absolute() {
    args.fixtures_root.clone()
  } else {
    crate::repo_root().join(&args.fixtures_root)
  };

  let include_scripts = args.include_scripts;

  if !fixtures_root.is_dir() {
    bail!(
      "fixtures root {} is not a directory",
      fixtures_root.display()
    );
  }

  let only: Option<BTreeSet<String>> = args
    .only
    .map(|fixtures| fixtures.into_iter().collect::<BTreeSet<_>>());

  let mut violations: Vec<Violation> = Vec::new();
  for entry in WalkDir::new(&fixtures_root) {
    let entry = entry.context("walk fixtures directory")?;
    if !entry.file_type().is_file() {
      continue;
    }

    let path = entry.path();
    let rel = match path.strip_prefix(&fixtures_root) {
      Ok(p) => p,
      Err(_) => continue,
    };
    let Some(fixture_name) = rel
      .components()
      .next()
      .and_then(|c| c.as_os_str().to_str())
      .map(str::to_string)
    else {
      continue;
    };

    if let Some(only) = &only {
      if !only.contains(&fixture_name) {
        continue;
      }
    }

    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let ext = path
      .extension()
      .and_then(|s| s.to_str())
      .unwrap_or("")
      .to_ascii_lowercase();

    enum Kind {
      Html,
      Css,
      Svg,
    }

    let kind = if file_name == "index.html" {
      Some(Kind::Html)
    } else if ext == "css" {
      Some(Kind::Css)
    } else if ext == "svg" {
      Some(Kind::Svg)
    } else {
      None
    };
    let Some(kind) = kind else {
      continue;
    };

    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let content = String::from_utf8_lossy(&bytes).to_string();

    let spans = match kind {
      Kind::Html => scan_html_for_remote_fetches(&content, include_scripts),
      Kind::Css => scan_css_for_remote_fetches(&content),
      Kind::Svg => scan_svg_for_remote_fetches(&content),
    };

    if spans.is_empty() {
      if matches!(kind, Kind::Html) {
        scan_embedded_html_assets_for_fixture(
          &fixtures_root,
          &fixture_name,
          path,
          &content,
          include_scripts,
          &mut violations,
        )?;
      }
      continue;
    }

    let line_index = LineIndex::new(&content);
    for span in spans {
      let (line, column) = line_index.line_col(span.start);
      violations.push(Violation {
        fixture: fixture_name.clone(),
        file: rel.to_path_buf(),
        line,
        column,
        url: span.url,
      });
    }

    if matches!(kind, Kind::Html) {
      scan_embedded_html_assets_for_fixture(
        &fixtures_root,
        &fixture_name,
        path,
        &content,
        include_scripts,
        &mut violations,
      )?;
    }
  }

  violations.sort_by(|a, b| {
    a.fixture
      .cmp(&b.fixture)
      .then_with(|| a.file.cmp(&b.file))
      .then_with(|| a.line.cmp(&b.line))
      .then_with(|| a.column.cmp(&b.column))
      .then_with(|| a.url.cmp(&b.url))
  });

  if violations.is_empty() {
    println!("✓ Page fixtures are offline (no remote fetchable references found).");
    return Ok(());
  }

  println!(
    "Found {} remote fetchable reference(s) in page fixtures:",
    violations.len()
  );
  for violation in &violations {
    println!(
      "  {}: {}:{}:{} {}",
      violation.fixture,
      violation.file.display(),
      violation.line,
      violation.column,
      violation.url
    );
  }

  bail!(
    "found {} remote fetchable reference(s); see report above",
    violations.len()
  )
}

fn is_remote_fetch_url(url: &str) -> bool {
  let lower = url.trim_start().to_ascii_lowercase();
  lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//")
}

fn is_html_extension(ext: &str) -> bool {
  matches!(ext, "html" | "htm" | "xhtml")
}

fn strip_query_fragment(url: &str) -> &str {
  let mut end = url.len();
  if let Some(idx) = url.find('#') {
    end = end.min(idx);
  }
  if let Some(idx) = url.find('?') {
    end = end.min(idx);
  }
  &url[..end]
}

fn resolve_local_html_path(fixture_dir: &Path, base_dir: &Path, raw: &str) -> Option<PathBuf> {
  let trimmed = raw.trim();
  if trimmed.is_empty() || is_remote_fetch_url(trimmed) {
    return None;
  }

  let lower = trimmed.to_ascii_lowercase();
  if lower.starts_with("data:")
    || lower.starts_with("about:")
    || lower.starts_with("javascript:")
    || lower.starts_with("mailto:")
    || lower.starts_with("tel:")
  {
    return None;
  }

  let trimmed = strip_query_fragment(trimmed);
  if trimmed.is_empty() {
    return None;
  }

  let base_url = Url::from_directory_path(base_dir).ok()?;
  let mut joined = base_url.join(trimmed).ok()?;
  joined.set_fragment(None);
  joined.set_query(None);
  if joined.scheme() != "file" {
    return None;
  }
  let path = joined.to_file_path().ok()?;
  if !path.starts_with(fixture_dir) {
    return None;
  }
  let ext = path
    .extension()
    .and_then(|e| e.to_str())?
    .to_ascii_lowercase();
  if !is_html_extension(&ext) {
    return None;
  }
  if !path.is_file() {
    return None;
  }
  Some(path)
}

fn capture_first_match<'t>(
  caps: &'t regex::Captures<'t>,
  groups: &[usize],
) -> Option<regex::Match<'t>> {
  groups.iter().find_map(|idx| caps.get(*idx))
}

fn extract_embedded_html_urls(html: &str) -> Vec<String> {
  static IFRAME_SRC: OnceLock<Regex> = OnceLock::new();
  static EMBED_SRC: OnceLock<Regex> = OnceLock::new();
  static OBJECT_DATA: OnceLock<Regex> = OnceLock::new();

  let html = mask_html_script_contents(html);

  let iframe_src = IFRAME_SRC.get_or_init(|| {
    Regex::new("(?is)<iframe[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("iframe src regex")
  });
  let embed_src = EMBED_SRC.get_or_init(|| {
    Regex::new("(?is)<embed[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("embed src regex")
  });
  let object_data = OBJECT_DATA.get_or_init(|| {
    Regex::new("(?is)<object[^>]*\\sdata\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("object data regex")
  });

  let mut out = Vec::new();
  for caps in iframe_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      out.push(m.as_str().to_string());
    }
  }
  for caps in embed_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      out.push(m.as_str().to_string());
    }
  }
  for caps in object_data.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      out.push(m.as_str().to_string());
    }
  }
  out
}

fn scan_embedded_html_assets_for_fixture(
  fixtures_root: &Path,
  fixture_name: &str,
  index_path: &Path,
  index_html: &str,
  include_scripts: bool,
  violations: &mut Vec<Violation>,
) -> Result<()> {
  let fixture_dir = fixtures_root.join(fixture_name);
  let base_dir = index_path.parent().unwrap_or_else(|| fixture_dir.as_path());

  let mut queue: VecDeque<PathBuf> = VecDeque::new();
  let mut visited: BTreeSet<PathBuf> = BTreeSet::new();

  for url in extract_embedded_html_urls(index_html) {
    if let Some(path) = resolve_local_html_path(&fixture_dir, base_dir, &url) {
      if visited.insert(path.clone()) {
        queue.push_back(path);
      }
    }
  }

  while let Some(path) = queue.pop_front() {
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let html = String::from_utf8_lossy(&bytes).to_string();

    let spans = scan_html_for_remote_fetches(&html, include_scripts);
    if !spans.is_empty() {
      let rel = path
        .strip_prefix(fixtures_root)
        .unwrap_or(&path)
        .to_path_buf();
      let line_index = LineIndex::new(&html);
      for span in spans {
        let (line, column) = line_index.line_col(span.start);
        violations.push(Violation {
          fixture: fixture_name.to_string(),
          file: rel.clone(),
          line,
          column,
          url: span.url,
        });
      }
    }

    let base_dir = path.parent().unwrap_or(&fixture_dir);
    for url in extract_embedded_html_urls(&html) {
      if let Some(next) = resolve_local_html_path(&fixture_dir, base_dir, &url) {
        if visited.insert(next.clone()) {
          queue.push_back(next);
        }
      }
    }
  }

  Ok(())
}

fn push_match_if_remote(out: &mut Vec<UrlSpan>, m: regex::Match<'_>) {
  let raw = m.as_str();
  let trimmed = raw.trim();
  if trimmed.is_empty() || !is_remote_fetch_url(trimmed) {
    return;
  }
  let leading = raw.find(trimmed).unwrap_or(0);
  let start = m.start() + leading;
  out.push(UrlSpan {
    url: trimmed.to_string(),
    start,
    end: start + trimmed.len(),
  });
}

fn scan_css_for_remote_fetches(css: &str) -> Vec<UrlSpan> {
  static URL_REGEX: OnceLock<Regex> = OnceLock::new();
  static IMPORT_REGEX: OnceLock<Regex> = OnceLock::new();
  static IMAGE_SET_REGEX: OnceLock<Regex> = OnceLock::new();
  static QUOTED_URL_IN_IMAGE_SET: OnceLock<Regex> = OnceLock::new();

  let url_regex = URL_REGEX.get_or_init(|| {
    Regex::new("(?i)(?P<prefix>url\\(\\s*[\"']?)(?P<url>[^\"')]+)(?P<suffix>[\"']?\\s*\\)?)")
      .expect("url regex must compile")
  });
  let import_regex = IMPORT_REGEX.get_or_init(|| {
    Regex::new("(?i)(?P<prefix>@import\\s*(?:url\\(\\s*)?[\"']?)(?P<url>[^\"')\\s;]+)")
      .expect("import regex must compile")
  });
  let image_set_regex = IMAGE_SET_REGEX.get_or_init(|| {
    Regex::new("(?i)(?:-webkit-)?image-set\\((?P<body>[^)]*)\\)")
      .expect("image-set regex must compile")
  });
  let quoted_url_in_image_set = QUOTED_URL_IN_IMAGE_SET.get_or_init(|| {
    // Note: Rust's regex crate does not support backreferences, so we don't attempt to ensure the
    // same quote is used on both sides.
    Regex::new("(?i)(?:^|[\\s,(])[\"'](?P<url>(?:https?://|//)[^\"']+)[\"']")
      .expect("quoted url in image-set regex must compile")
  });

  fn is_css_namespace_rule_prefix(content: &str, at: usize) -> bool {
    let bytes = content.as_bytes();
    let mut start = at;
    while start > 0 {
      match bytes[start - 1] {
        b';' | b'{' | b'}' => break,
        _ => start -= 1,
      }
    }
    content[start..at]
      .to_ascii_lowercase()
      .contains("@namespace")
  }

  let mut out = Vec::new();
  for caps in url_regex.captures_iter(css) {
    if let Some(m) = caps.name("url") {
      // `@namespace url("http://www.w3.org/...")` is not a fetchable resource URL.
      if is_css_namespace_rule_prefix(css, m.start()) {
        continue;
      }
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in import_regex.captures_iter(css) {
    if let Some(m) = caps.name("url") {
      push_match_if_remote(&mut out, m);
    }
  }

  // `image-set("https://...", url(...))` syntax.
  for caps in image_set_regex.captures_iter(css) {
    let Some(body) = caps.name("body") else {
      continue;
    };
    for inner in quoted_url_in_image_set.captures_iter(body.as_str()) {
      let Some(m) = inner.name("url") else {
        continue;
      };
      let start = body.start() + m.start();
      out.push(UrlSpan {
        url: m.as_str().to_string(),
        start,
        end: body.start() + m.end(),
      });
    }
  }

  out
}

fn parse_srcset_urls(srcset: &str, max_candidates: usize) -> Vec<String> {
  fastrender::html::image_attrs::parse_srcset_with_limit(srcset, max_candidates)
    .into_iter()
    .map(|candidate| candidate.url)
    .collect()
}

fn push_srcset_violations(out: &mut Vec<UrlSpan>, value: &str, value_start: usize, max: usize) {
  let candidates = parse_srcset_urls(value, max);
  if candidates.is_empty() {
    return;
  }

  let mut search_start = 0usize;
  for candidate in candidates {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || !is_remote_fetch_url(trimmed) {
      continue;
    }

    let needle = trimmed;
    let found = value[search_start..].find(needle);
    let offset_in_value = match found {
      Some(pos) => {
        let absolute = search_start + pos;
        search_start = absolute + needle.len();
        absolute
      }
      None => value.find(needle).unwrap_or(0),
    };
    let start = value_start + offset_in_value;
    out.push(UrlSpan {
      url: needle.to_string(),
      start,
      end: start + needle.len(),
    });
  }
}

fn scan_html_for_remote_fetches(html: &str, include_scripts: bool) -> Vec<UrlSpan> {
  static IMG_SRC: OnceLock<Regex> = OnceLock::new();
  static IFRAME_SRC: OnceLock<Regex> = OnceLock::new();
  static EMBED_SRC: OnceLock<Regex> = OnceLock::new();
  static OBJECT_DATA: OnceLock<Regex> = OnceLock::new();
  static VIDEO_POSTER: OnceLock<Regex> = OnceLock::new();
  static VIDEO_SRC: OnceLock<Regex> = OnceLock::new();
  static AUDIO_SRC: OnceLock<Regex> = OnceLock::new();
  static TRACK_SRC: OnceLock<Regex> = OnceLock::new();
  static SOURCE_SRC: OnceLock<Regex> = OnceLock::new();
  static IMG_SRCSET: OnceLock<Regex> = OnceLock::new();
  static SOURCE_SRCSET: OnceLock<Regex> = OnceLock::new();
  static SCRIPT_SRC: OnceLock<Regex> = OnceLock::new();
  static STYLE_TAG: OnceLock<Regex> = OnceLock::new();
  static STYLE_ATTR_DOUBLE: OnceLock<Regex> = OnceLock::new();
  static STYLE_ATTR_SINGLE: OnceLock<Regex> = OnceLock::new();
  static LINK_TAG: OnceLock<Regex> = OnceLock::new();
  static ATTR_REL: OnceLock<Regex> = OnceLock::new();
  static ATTR_HREF: OnceLock<Regex> = OnceLock::new();
  static ATTR_IMAGESRCSET: OnceLock<Regex> = OnceLock::new();

  let img_src = IMG_SRC.get_or_init(|| {
    Regex::new("(?is)<img[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("img src regex")
  });
  let iframe_src = IFRAME_SRC.get_or_init(|| {
    Regex::new("(?is)<iframe[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("iframe src regex")
  });
  let embed_src = EMBED_SRC.get_or_init(|| {
    Regex::new("(?is)<embed[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("embed src regex")
  });
  let object_data = OBJECT_DATA.get_or_init(|| {
    Regex::new("(?is)<object[^>]*\\sdata\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("object data regex")
  });
  let video_poster = VIDEO_POSTER.get_or_init(|| {
    Regex::new("(?is)<video[^>]*\\sposter\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("video poster regex")
  });
  let video_src = VIDEO_SRC.get_or_init(|| {
    Regex::new("(?is)<video[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("video src regex")
  });
  let audio_src = AUDIO_SRC.get_or_init(|| {
    Regex::new("(?is)<audio[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("audio src regex")
  });
  let track_src = TRACK_SRC.get_or_init(|| {
    Regex::new("(?is)<track[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("track src regex")
  });
  let source_src = SOURCE_SRC.get_or_init(|| {
    Regex::new("(?is)<source[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("source src regex")
  });
  let img_srcset = IMG_SRCSET.get_or_init(|| {
    Regex::new("(?is)<img[^>]*\\ssrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
      .expect("img srcset regex")
  });
  let source_srcset = SOURCE_SRCSET.get_or_init(|| {
    Regex::new("(?is)<source[^>]*\\ssrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
      .expect("source srcset regex")
  });
  let script_src = SCRIPT_SRC.get_or_init(|| {
    Regex::new("(?is)<script[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("script src regex")
  });
  let style_tag = STYLE_TAG
    .get_or_init(|| Regex::new("(?is)<style[^>]*>(.*?)</style>").expect("style tag regex"));
  let style_attr_double =
    STYLE_ATTR_DOUBLE.get_or_init(|| Regex::new("(?is)\\sstyle\\s*=\\s*\"([^\"]*)\"").unwrap());
  let style_attr_single =
    STYLE_ATTR_SINGLE.get_or_init(|| Regex::new("(?is)\\sstyle\\s*=\\s*'([^']*)'").unwrap());
  let link_tag = LINK_TAG.get_or_init(|| Regex::new("(?is)<link\\b[^>]*>").unwrap());
  let attr_rel = ATTR_REL.get_or_init(|| {
    Regex::new("(?is)(?:^|\\s)rel\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))").unwrap()
  });
  let attr_href = ATTR_HREF.get_or_init(|| {
    Regex::new("(?is)(?:^|\\s)href\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))").unwrap()
  });
  let attr_imagesrcset = ATTR_IMAGESRCSET.get_or_init(|| {
    Regex::new("(?is)(?:^|\\s)imagesrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')").unwrap()
  });

  let mut out = Vec::new();
  let html = mask_html_script_contents(html);

  for caps in img_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in iframe_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in embed_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in object_data.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in video_poster.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in video_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in audio_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in track_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in source_src.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }

  const MAX_SRCSET_CANDIDATES: usize = 64;
  for caps in img_srcset.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2]) {
      push_srcset_violations(&mut out, m.as_str(), m.start(), MAX_SRCSET_CANDIDATES);
    }
  }
  for caps in source_srcset.captures_iter(html.as_ref()) {
    if let Some(m) = capture_first_match(&caps, &[1, 2]) {
      push_srcset_violations(&mut out, m.as_str(), m.start(), MAX_SRCSET_CANDIDATES);
    }
  }

  if include_scripts {
    for caps in script_src.captures_iter(html.as_ref()) {
      if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
        push_match_if_remote(&mut out, m);
      }
    }
  }

  // Inline CSS.
  for caps in style_tag.captures_iter(html.as_ref()) {
    let Some(css_match) = caps.get(1) else {
      continue;
    };
    for span in scan_css_for_remote_fetches(css_match.as_str()) {
      out.push(UrlSpan {
        url: span.url,
        start: css_match.start() + span.start,
        end: css_match.start() + span.end,
      });
    }
  }
  for caps in style_attr_double.captures_iter(html.as_ref()) {
    let Some(css_match) = caps.get(1) else {
      continue;
    };
    for span in scan_css_for_remote_fetches(css_match.as_str()) {
      out.push(UrlSpan {
        url: span.url,
        start: css_match.start() + span.start,
        end: css_match.start() + span.end,
      });
    }
  }
  for caps in style_attr_single.captures_iter(html.as_ref()) {
    let Some(css_match) = caps.get(1) else {
      continue;
    };
    for span in scan_css_for_remote_fetches(css_match.as_str()) {
      out.push(UrlSpan {
        url: span.url,
        start: css_match.start() + span.start,
        end: css_match.start() + span.end,
      });
    }
  }

  // Fetchable <link href> and <link imagesrcset>.
  for tag_match in link_tag.captures_iter(html.as_ref()) {
    let Some(tag) = tag_match.get(0) else {
      continue;
    };
    let tag_str = tag.as_str();
    let rel_value = attr_rel
      .captures(tag_str)
      .and_then(|c| capture_first_match(&c, &[1, 2, 3]).map(|m| m.as_str().to_string()));
    let Some(rel_value) = rel_value else {
      continue;
    };

    let mut is_fetchable = false;
    for token in rel_value.split_ascii_whitespace() {
      if token.eq_ignore_ascii_case("stylesheet")
        || token.eq_ignore_ascii_case("preload")
        || (include_scripts && token.eq_ignore_ascii_case("modulepreload"))
        || token.eq_ignore_ascii_case("prefetch")
        || token.eq_ignore_ascii_case("icon")
        || token.eq_ignore_ascii_case("apple-touch-icon")
        || token.eq_ignore_ascii_case("apple-touch-icon-precomposed")
        || token.eq_ignore_ascii_case("manifest")
        || token.eq_ignore_ascii_case("mask-icon")
        || token.eq_ignore_ascii_case("preconnect")
        || token.eq_ignore_ascii_case("dns-prefetch")
      {
        is_fetchable = true;
        break;
      }
    }
    if !is_fetchable {
      continue;
    }

    // href=
    if let Some(href_caps) = attr_href.captures(tag_str) {
      if let Some(href_match) = capture_first_match(&href_caps, &[1, 2, 3]) {
        let start = tag.start() + href_match.start();
        out.push(UrlSpan {
          url: href_match.as_str().trim().to_string(),
          start,
          end: tag.start() + href_match.end(),
        });
      }
    }

    // imagesrcset=
    if let Some(srcset_caps) = attr_imagesrcset.captures(tag_str) {
      if let Some(value_match) = capture_first_match(&srcset_caps, &[1, 2]) {
        push_srcset_violations(
          &mut out,
          value_match.as_str(),
          tag.start() + value_match.start(),
          MAX_SRCSET_CANDIDATES,
        );
      }
    }

    // Remove any non-remote href values recorded above.
    // (We need this post-filter because we captured href values without checking scheme.)
  }

  // Filter link href captures to only remote URLs.
  out.retain(|span| is_remote_fetch_url(&span.url));

  out
}

fn mask_html_script_contents(html: &str) -> Cow<'_, str> {
  // The validator uses regexes for deterministic scanning rather than a full HTML parser. To avoid
  // false positives (e.g. "<img src=...>" strings inside inline JS), treat script element content
  // as raw text and mask it out before running regexes.
  const SCRIPT: &[u8] = b"script";

  fn is_boundary_byte(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\r' | b'\t' | b'/' | b'>')
  }

  fn eq_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
      .iter()
      .zip(needle.iter())
      .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
  }

  fn find_tag_end(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
      match bytes[i] {
        b'\'' if !in_double => in_single = !in_single,
        b'"' if !in_single => in_double = !in_double,
        b'>' if !in_single && !in_double => return Some(i),
        _ => {}
      }
      i += 1;
    }
    None
  }

  let bytes = html.as_bytes();
  let mut out: Option<Vec<u8>> = None;
  let mut i = 0usize;
  while i < bytes.len() {
    if bytes[i] != b'<' || i + 1 + SCRIPT.len() > bytes.len() {
      i += 1;
      continue;
    }
    let name_start = i + 1;
    let name_end = name_start + SCRIPT.len();
    if !eq_ignore_ascii_case(&bytes[name_start..name_end], SCRIPT) {
      i += 1;
      continue;
    }
    if name_end < bytes.len() && !is_boundary_byte(bytes[name_end]) {
      i += 1;
      continue;
    }

    let Some(start_tag_end) = find_tag_end(bytes, name_end) else {
      break;
    };

    let content_start = start_tag_end + 1;
    let mut j = content_start;
    let mut end_tag_start: Option<usize> = None;
    while j + 2 + SCRIPT.len() <= bytes.len() {
      if bytes[j] == b'<'
        && j + 2 + SCRIPT.len() <= bytes.len()
        && bytes.get(j + 1) == Some(&b'/')
        && eq_ignore_ascii_case(&bytes[j + 2..j + 2 + SCRIPT.len()], SCRIPT)
      {
        let after = j + 2 + SCRIPT.len();
        if after == bytes.len() || is_boundary_byte(*bytes.get(after).unwrap_or(&b'>')) {
          end_tag_start = Some(j);
          break;
        }
      }
      j += 1;
    }

    let mask_end = end_tag_start.unwrap_or(bytes.len());
    if mask_end > content_start {
      let vec = out.get_or_insert_with(|| bytes.to_vec());
      for b in &mut vec[content_start..mask_end] {
        if *b != b'\n' && *b != b'\r' {
          *b = b' ';
        }
      }
    }

    match end_tag_start {
      Some(pos) => i = pos + 1,
      None => break,
    }
  }

  match out {
    Some(vec) => Cow::Owned(String::from_utf8(vec).expect("masked HTML must remain valid UTF-8")),
    None => Cow::Borrowed(html),
  }
}

fn scan_svg_for_remote_fetches(svg: &str) -> Vec<UrlSpan> {
  static IMAGE_HREF: OnceLock<Regex> = OnceLock::new();
  static USE_HREF: OnceLock<Regex> = OnceLock::new();
  static FEIMAGE_HREF: OnceLock<Regex> = OnceLock::new();

  let image_href = IMAGE_HREF.get_or_init(|| {
    Regex::new(
      "(?is)<image[^>]*\\s(?:href|xlink:href)\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
    )
    .unwrap()
  });
  let use_href = USE_HREF.get_or_init(|| {
    Regex::new("(?is)<use[^>]*\\s(?:href|xlink:href)\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .unwrap()
  });
  let feimage_href = FEIMAGE_HREF.get_or_init(|| {
    Regex::new(
      "(?is)<feimage[^>]*\\s(?:href|xlink:href)\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
    )
    .unwrap()
  });

  let mut out = Vec::new();
  for caps in image_href.captures_iter(svg) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in use_href.captures_iter(svg) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }
  for caps in feimage_href.captures_iter(svg) {
    if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
      push_match_if_remote(&mut out, m);
    }
  }

  // Also scan for `url(...)` / `@import` style references embedded anywhere in the SVG (style tags,
  // style= attributes, and presentation attributes such as fill="url(...)").
  out.extend(scan_css_for_remote_fetches(svg));
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeSet;

  #[test]
  fn script_subresource_validation_is_opt_in() {
    let html = r#"
<!doctype html>
<html>
  <head>
    <link rel="preload" as="script" href="https://example.com/preload.js">
    <link rel="modulepreload" href="https://example.com/module.js">
    <script>
      // Strings that look like markup must not trigger validator matches.
      const html = '<script src="https://example.com/inner.js">';
    </script>
    <script src="https://example.com/outer.js"></script>
    <script src="javascript:alert(1)"></script>
    <script src="data:text/javascript,alert(1)"></script>
  </head>
</html>
"#;

    let off: BTreeSet<String> = scan_html_for_remote_fetches(html, false)
      .into_iter()
      .map(|span| span.url)
      .collect();
    assert!(off.contains("https://example.com/preload.js"));
    assert!(!off.contains("https://example.com/outer.js"));
    assert!(!off.contains("https://example.com/module.js"));
    assert!(!off.contains("https://example.com/inner.js"));
    assert!(!off.contains("javascript:alert(1)"));
    assert!(!off.contains("data:text/javascript,alert(1)"));

    let on: BTreeSet<String> = scan_html_for_remote_fetches(html, true)
      .into_iter()
      .map(|span| span.url)
      .collect();
    assert!(on.contains("https://example.com/preload.js"));
    assert!(on.contains("https://example.com/module.js"));
    assert!(on.contains("https://example.com/outer.js"));
    assert!(!on.contains("https://example.com/inner.js"));
    assert!(!on.contains("javascript:alert(1)"));
    assert!(!on.contains("data:text/javascript,alert(1)"));
  }
}
