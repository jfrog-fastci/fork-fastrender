//! Offline invariants for the vendored Web Platform Tests corpus.
//!
//! The WPT runner is intended to run fully offline. The importer (`src/bin/import_wpt.rs`)
//! already validates offline invariants for imported files, but tests can also be added/edited
//! manually. This test fails fast (with file/line/column diagnostics) if any test references
//! remote-fetchable URLs.
//!
//! Invariants:
//! - No `http://`, `https://`, or protocol-relative `//...` URLs in fetchable contexts.
//! - Ignore XML/SVG namespace declarations like `xmlns="http://www.w3.org/2000/svg"`.
//! - Ignore `data:` URLs.

use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::WalkDir;

use fastrender::html::image_attrs::parse_srcset_with_limit;

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
  // Keep patterns in sync with `src/bin/import_wpt.rs::find_network_urls` where possible. This
  // keeps the test corpus constraints aligned with what the renderer/harness can actually fetch.
  attr_quoted: Regex,
  attr_unquoted: Regex,
  css_url: Regex,
  css_import: Regex,
  link_tag: Regex,
  link_rel: Regex,
  link_href: Regex,
  svg_href: Regex,
  xlink_href: Regex,
  srcset_double: Regex,
  srcset_single: Regex,
  srcset_unquoted: Regex,
  imagesrcset_double: Regex,
  imagesrcset_single: Regex,
  imagesrcset_unquoted: Regex,
}

impl Scanner {
  fn new() -> Self {
    Self {
      attr_quoted: Regex::new(r#"(?i)\s(?:src|poster|data)\s*=\s*["'](?P<url>[^"'>]+)["']"#)
        .unwrap(),
      attr_unquoted: Regex::new(r#"(?i)\s(?:src|poster|data)\s*=\s*(?P<url>[^\s"'>]+)"#).unwrap(),
      css_url: Regex::new(r#"(?i)url\(\s*["']?(?P<url>[^"')]+)["']?\s*\)"#).unwrap(),
      css_import: Regex::new(r#"(?i)@import\s+["'](?P<url>[^"']+)["']"#).unwrap(),
      link_tag: Regex::new(r#"(?is)<link\b[^>]*>"#).unwrap(),
      link_rel: Regex::new(
        r#"(?is)(?:^|\s)rel\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
      )
      .unwrap(),
      link_href: Regex::new(
        r#"(?is)(?:^|\s)href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
      )
      .unwrap(),
      svg_href: Regex::new(
        r#"(?is)<(?:image|use|feimage)\b[^>]*\shref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
      )
      .unwrap(),
      xlink_href: Regex::new(
        r#"(?is)<(?P<tag>[a-z0-9:_-]+)\b[^>]*\sxlink:href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
      )
      .unwrap(),
      srcset_double: Regex::new(r#"(?i)\ssrcset\s*=\s*"(?P<value>[^"]*)""#).unwrap(),
      srcset_single: Regex::new(r#"(?i)\ssrcset\s*=\s*'(?P<value>[^']*)'"#).unwrap(),
      srcset_unquoted: Regex::new(r#"(?i)\ssrcset\s*=\s*(?P<value>[^\s"'>]+)"#).unwrap(),
      imagesrcset_double: Regex::new(r#"(?i)\simagesrcset\s*=\s*"(?P<value>[^"]*)""#).unwrap(),
      imagesrcset_single: Regex::new(r#"(?i)\simagesrcset\s*=\s*'(?P<value>[^']*)'"#).unwrap(),
      imagesrcset_unquoted: Regex::new(r#"(?i)\simagesrcset\s*=\s*(?P<value>[^\s"'>]+)"#)
        .unwrap(),
    }
  }

  fn find_network_urls(&self, content: &str) -> Vec<UrlMatch> {
    fn is_network_url(value: &str) -> bool {
      let trimmed = value.trim();
      trimmed
        .get(..7)
        .map(|prefix| prefix.eq_ignore_ascii_case("http://"))
        .unwrap_or(false)
        || trimmed
          .get(..8)
          .map(|prefix| prefix.eq_ignore_ascii_case("https://"))
          .unwrap_or(false)
        || trimmed.starts_with("//")
    }

    fn network_url_trim_span(value: &str) -> Option<(usize, usize)> {
      let start_trimmed = value.trim_start();
      let leading = value.len() - start_trimmed.len();
      let trimmed = start_trimmed.trim_end();
      is_network_url(trimmed).then_some((leading, leading + trimmed.len()))
    }

    fn link_rel_requires_fetch(rel: &str) -> bool {
      rel.split_ascii_whitespace().any(|token| {
        token.eq_ignore_ascii_case("stylesheet")
          || token.eq_ignore_ascii_case("preload")
          || token.eq_ignore_ascii_case("modulepreload")
          || token.eq_ignore_ascii_case("match")
          || token.eq_ignore_ascii_case("mismatch")
          || token.eq_ignore_ascii_case("icon")
          || token.eq_ignore_ascii_case("mask-icon")
          || token.eq_ignore_ascii_case("manifest")
      })
    }

    let mut matches = Vec::new();

    for regex in [&self.attr_quoted, &self.attr_unquoted, &self.css_import] {
      for caps in regex.captures_iter(content) {
        let Some(m) = caps.name("url") else {
          continue;
        };
        let Some((rel_start, rel_end)) = network_url_trim_span(m.as_str()) else {
          continue;
        };
        matches.push(UrlMatch {
          start: m.start() + rel_start,
          end: m.start() + rel_end,
          url: m.as_str()[rel_start..rel_end].to_string(),
        });
      }
    }

    // `href` is only fetchable in a subset of contexts (notably `<link>`). Avoid flagging common
    // WPT metadata like `<link rel=help href="https://...">` by only validating the `href` value
    // when the `<link rel>` indicates a fetchable relation.
    for tag_match in self.link_tag.find_iter(content) {
      let tag = tag_match.as_str();
      let Some(rel_caps) = self.link_rel.captures(tag) else {
        continue;
      };
      let rel_value = rel_caps
        .get(1)
        .or_else(|| rel_caps.get(2))
        .or_else(|| rel_caps.get(3))
        .map(|m| m.as_str())
        .unwrap_or("");
      if !link_rel_requires_fetch(rel_value) {
        continue;
      }

      let Some(href_caps) = self.link_href.captures(tag) else {
        continue;
      };
      let Some(href_match) = href_caps
        .get(1)
        .or_else(|| href_caps.get(2))
        .or_else(|| href_caps.get(3))
      else {
        continue;
      };
      let Some((rel_start, rel_end)) = network_url_trim_span(href_match.as_str()) else {
        continue;
      };
      matches.push(UrlMatch {
        start: tag_match.start() + href_match.start() + rel_start,
        end: tag_match.start() + href_match.start() + rel_end,
        url: href_match.as_str()[rel_start..rel_end].to_string(),
      });
    }

    // SVG fetch contexts commonly use `href=` (SVG2) or `xlink:href=` (SVG1). We already scan
    // `href` would otherwise be missed when we avoid flagging non-fetchable HTML anchors and
    // metadata links.
    for caps in self.svg_href.captures_iter(content) {
      let Some(m) = caps.get(1).or_else(|| caps.get(2)).or_else(|| caps.get(3)) else {
        continue;
      };
      let Some((rel_start, rel_end)) = network_url_trim_span(m.as_str()) else {
        continue;
      };
      matches.push(UrlMatch {
        start: m.start() + rel_start,
        end: m.start() + rel_end,
        url: m.as_str()[rel_start..rel_end].to_string(),
      });
    }

    // For `xlink:href`, ignore `<a xlink:href>` navigation links but treat all other tags as
    // potential resource references (e.g. `<image>`, gradients, patterns, filters).
    for caps in self.xlink_href.captures_iter(content) {
      let tag = caps.name("tag").map(|m| m.as_str()).unwrap_or("");
      if tag.eq_ignore_ascii_case("a") {
        continue;
      }
      let Some(m) = caps.get(2).or_else(|| caps.get(3)).or_else(|| caps.get(4)) else {
        continue;
      };
      let Some((rel_start, rel_end)) = network_url_trim_span(m.as_str()) else {
        continue;
      };
      matches.push(UrlMatch {
        start: m.start() + rel_start,
        end: m.start() + rel_end,
        url: m.as_str()[rel_start..rel_end].to_string(),
      });
    }

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

    for caps in self.css_url.captures_iter(content) {
      let Some(url_match) = caps.name("url") else {
        continue;
      };
      // `@namespace url("http://www.w3.org/...")` is not a fetchable resource URL.
      if is_css_namespace_rule_prefix(content, url_match.start()) {
        continue;
      }
      let Some((rel_start, rel_end)) = network_url_trim_span(url_match.as_str()) else {
        continue;
      };
      matches.push(UrlMatch {
        start: url_match.start() + rel_start,
        end: url_match.start() + rel_end,
        url: url_match.as_str()[rel_start..rel_end].to_string(),
      });
    }

    const MAX_SRCSET_CANDIDATES: usize = 64;
    for regex in [
      &self.srcset_double,
      &self.srcset_single,
      &self.srcset_unquoted,
      &self.imagesrcset_double,
      &self.imagesrcset_single,
      &self.imagesrcset_unquoted,
    ] {
      for caps in regex.captures_iter(content) {
        let Some(raw_srcset_match) = caps.name("value") else {
          continue;
        };
        let raw_srcset = raw_srcset_match.as_str();
        let candidates = parse_srcset_with_limit(raw_srcset, MAX_SRCSET_CANDIDATES);

        // Locate candidate URLs in the raw string to provide per-URL diagnostics. We prefer a
        // best-effort substring search over re-parsing `srcset` ourselves.
        let mut search_from = 0usize;
        for candidate in candidates {
          if !is_network_url(&candidate.url) {
            continue;
          }

          let rel_pos = raw_srcset[search_from..].find(&candidate.url);
          let abs_start = match rel_pos {
            Some(pos) => raw_srcset_match.start() + search_from + pos,
            None => raw_srcset_match.start(),
          };
          let abs_end = abs_start + candidate.url.len();
          matches.push(UrlMatch {
            start: abs_start,
            end: abs_end,
            url: candidate.url,
          });

          if let Some(pos) = rel_pos {
            search_from = search_from + pos + (abs_end - abs_start);
          }
        }
      }
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

    for m in scanner.find_network_urls(&content) {
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
