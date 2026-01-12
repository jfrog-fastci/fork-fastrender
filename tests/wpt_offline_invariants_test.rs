//! Offline invariants for the vendored Web Platform Tests corpus.
//!
//! The WPT runner is intended to run fully offline. The importer (`src/bin/import_wpt.rs`)
//! already validates offline invariants for imported files, but tests can also be added/edited
//! manually. This test fails fast (with file/line/column diagnostics) if any test references
//! remote-fetchable URLs.
//!
//! Invariants:
//! - No `http://`, `https://`, or protocol-relative `//...` URLs.
//! - Ignore XML/SVG namespace declarations like `xmlns="http://www.w3.org/2000/svg"`.
//! - Ignore `data:` URLs.

use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
  path: PathBuf,
  line: usize,
  column: usize,
  url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlMatch {
  start: usize,
  end: usize,
  url: String,
}

struct Scanner {
  // Best-effort spans we want to ignore while doing a strict scan for network-looking URLs.
  //
  // This mirrors the approach in `src/bin/import_wpt.rs::find_network_urls_strict`, but we extend
  // it to also ignore XML namespace declarations (which are not fetchable resources).
  data_url_span_double_quoted: Regex,
  data_url_span_single_quoted: Regex,
  data_url_span_css_url: Regex,
  xml_namespace_attr: Regex,
  http_url: Regex,
  scheme_relative_url: Regex,
}

impl Scanner {
  fn new() -> Self {
    Self {
      data_url_span_double_quoted: Regex::new(r#"(?is)"(?P<url>data:[^"]*)""#).unwrap(),
      data_url_span_single_quoted: Regex::new(r#"(?is)'(?P<url>data:[^']*)'"#).unwrap(),
      data_url_span_css_url: Regex::new(r#"(?is)url\(\s*(?P<url>data:[^)]*)\)"#).unwrap(),
      xml_namespace_attr: Regex::new(
        r#"(?is)\bxmlns(?::[a-z0-9._-]+)?\s*=\s*(?:"(?P<dq>[^"]*)"|'(?P<sq>[^']*)'|(?P<uq>[^\s>]+))"#,
      )
      .unwrap(),
      http_url: Regex::new(r#"(?i)https?://[^\s"'<>)]{1,200}"#).unwrap(),
      scheme_relative_url: Regex::new(r#"(?i)//[^\s"'<>)]{1,200}"#).unwrap(),
    }
  }

  fn data_url_spans(&self, content: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();

    for caps in self.data_url_span_double_quoted.captures_iter(content) {
      if let Some(m) = caps.name("url") {
        spans.push(m.start()..m.end());
      }
    }
    for caps in self.data_url_span_single_quoted.captures_iter(content) {
      if let Some(m) = caps.name("url") {
        spans.push(m.start()..m.end());
      }
    }
    for caps in self.data_url_span_css_url.captures_iter(content) {
      if let Some(m) = caps.name("url") {
        spans.push(m.start()..m.end());
      }
    }

    spans.sort_by_key(|span| span.start);
    spans
  }

  fn xml_namespace_spans(&self, content: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    for caps in self.xml_namespace_attr.captures_iter(content) {
      let Some(m) = caps
        .name("dq")
        .or_else(|| caps.name("sq"))
        .or_else(|| caps.name("uq"))
      else {
        continue;
      };
      spans.push(m.start()..m.end());
    }
    spans.sort_by_key(|span| span.start);
    spans
  }

  fn is_within_spans(spans: &[std::ops::Range<usize>], idx: usize) -> bool {
    // Small N; linear scan is fine and keeps the implementation trivial.
    spans.iter().any(|span| idx >= span.start && idx < span.end)
  }

  fn find_network_urls_strict(&self, content: &str) -> Vec<UrlMatch> {
    let data_spans = self.data_url_spans(content);
    let namespace_spans = self.xml_namespace_spans(content);
    let content_bytes = content.as_bytes();

    let mut matches = Vec::new();

    for m in self.http_url.find_iter(content) {
      let idx = m.start();
      if Self::is_within_spans(&data_spans, idx) || Self::is_within_spans(&namespace_spans, idx) {
        continue;
      }
      matches.push(UrlMatch {
        start: m.start(),
        end: m.end(),
        url: m.as_str().to_string(),
      });
    }

    for m in self.scheme_relative_url.find_iter(content) {
      // Don't treat the `//` in `http://...` as a protocol-relative URL.
      if m.start() > 0 && content_bytes[m.start() - 1] == b':' {
        continue;
      }
      let idx = m.start();
      if Self::is_within_spans(&data_spans, idx) || Self::is_within_spans(&namespace_spans, idx) {
        continue;
      }
      matches.push(UrlMatch {
        start: m.start(),
        end: m.end(),
        url: m.as_str().to_string(),
      });
    }

    matches.sort_by(|a, b| (a.start, a.end, &a.url).cmp(&(b.start, b.end, &b.url)));
    matches.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.url == b.url);
    matches
  }
}

fn is_target_file(path: &Path) -> bool {
  let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
    return false;
  };
  matches!(
    ext.to_ascii_lowercase().as_str(),
    "html" | "htm" | "css" | "svg" | "xhtml"
  )
}

fn line_col_from_offset(content: &str, offset: usize) -> (usize, usize) {
  let prefix = &content[..offset.min(content.len())];
  let line = prefix.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1;
  let last_nl = prefix.as_bytes().iter().rposition(|&b| b == b'\n');
  let col = match last_nl {
    Some(idx) => prefix.len() - idx,
    None => prefix.len() + 1,
  };
  (line, col)
}

fn scan_dir(root: &Path) -> Vec<Violation> {
  let scanner = Scanner::new();
  let mut violations = Vec::new();

  for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
    if !entry.file_type().is_file() {
      continue;
    }
    let path = entry.path();
    if !is_target_file(path) {
      continue;
    }

    let bytes = match std::fs::read(path) {
      Ok(b) => b,
      Err(err) => panic!("failed to read {}: {err}", path.display()),
    };
    let content = String::from_utf8_lossy(&bytes);

    for m in scanner.find_network_urls_strict(&content) {
      let (line, column) = line_col_from_offset(&content, m.start);
      violations.push(Violation {
        path: path.to_path_buf(),
        line,
        column,
        url: m.url,
      });
    }
  }

  violations
}

#[test]
fn validator_unit_fixtures() {
  let root = Path::new("tests/wpt/_offline_validator_testdata");
  let violations = scan_dir(root);

  assert_eq!(violations.len(), 1, "unexpected violations: {violations:#?}");
  let v = &violations[0];
  assert!(
    v.path
      .to_string_lossy()
      .ends_with("_offline_validator_testdata/remote_fetch.html"),
    "unexpected path: {}",
    v.path.display()
  );
  assert_eq!(v.line, 2);
  assert_eq!(v.column, 30);
  assert_eq!(v.url, "https://example.com/style.css");
}

#[test]
fn wpt_corpus_has_no_network_urls() {
  let root = Path::new("tests/wpt/tests");
  let violations = scan_dir(root);

  if violations.is_empty() {
    return;
  }

  eprintln!("Offline WPT invariant violations (network URLs are not allowed):");
  for v in &violations {
    eprintln!("  {}:{}:{}: {}", v.path.display(), v.line, v.column, v.url);
  }
  panic!(
    "Found {} network URL(s) under {}",
    violations.len(),
    root.display()
  );
}
