use regex::Regex;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn corpus_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../tests/wpt_dom")
    .canonicalize()
    .expect("canonicalize corpus root")
}

fn tests_root() -> PathBuf {
  corpus_root().join("tests")
}

fn resources_root() -> PathBuf {
  corpus_root().join("resources")
}

fn normalize_url_path(path: &str) -> String {
  let mut stack: Vec<&str> = Vec::new();
  for comp in path.split('/') {
    if comp.is_empty() || comp == "." {
      continue;
    }
    if comp == ".." {
      stack.pop();
      continue;
    }
    stack.push(comp);
  }
  if stack.is_empty() {
    "/".to_string()
  } else {
    format!("/{}", stack.join("/"))
  }
}

fn base_url_dir(url_path: &str) -> String {
  let url_path = if url_path.starts_with('/') { url_path } else { "/" };
  match url_path.rsplit_once('/') {
    Some(("", _)) => "/".to_string(),
    Some((dir, _)) => format!("{dir}/"),
    None => "/".to_string(),
  }
}

fn resolve_relative_url_path(base_url_path: &str, relative: &str) -> String {
  let base_dir = base_url_dir(base_url_path);
  let joined = if base_dir == "/" {
    format!("/{relative}")
  } else {
    format!("{base_dir}{relative}")
  };
  normalize_url_path(&joined)
}

fn split_path_and_suffix(value: &str) -> (&str, &str) {
  if let Some(pos) = value.find(|c| c == '?' || c == '#') {
    (&value[..pos], &value[pos..])
  } else {
    (value, "")
  }
}

fn is_ignored_url(value: &str) -> bool {
  const DATA_URL_PREFIX: &str = "data:";
  let trimmed = value.trim();
  trimmed.is_empty()
    || trimmed.starts_with('#')
    || trimmed
      .get(..DATA_URL_PREFIX.len())
      .map(|prefix| prefix.eq_ignore_ascii_case(DATA_URL_PREFIX))
      .unwrap_or(false)
    || trimmed.starts_with("about:")
    || trimmed.starts_with("javascript:")
    || trimmed
      .get(..7)
      .map(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
      .unwrap_or(false)
    || trimmed
      .get(..4)
      .map(|prefix| prefix.eq_ignore_ascii_case("tel:"))
      .unwrap_or(false)
}

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

fn link_rel_requires_fetch(rel: &str) -> bool {
  rel.split_ascii_whitespace().any(|token| {
    token.eq_ignore_ascii_case("stylesheet")
      || token.eq_ignore_ascii_case("preload")
      || token.eq_ignore_ascii_case("modulepreload")
      || token.eq_ignore_ascii_case("icon")
      || token.eq_ignore_ascii_case("mask-icon")
      || token.eq_ignore_ascii_case("manifest")
  })
}

fn is_text_file(path: &Path) -> bool {
  let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
  if file_name.ends_with(".window.js")
    || file_name.ends_with(".any.js")
    || file_name.ends_with(".js")
    || file_name.ends_with(".css")
  {
    return true;
  }
  match path.extension().and_then(|e| e.to_str()) {
    Some("html") | Some("htm") => true,
    _ => false,
  }
}

fn read_text(path: &Path) -> String {
  std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn extract_js_meta_scripts(source: &str) -> Vec<String> {
  let mut scripts = Vec::new();
  for line in source.lines() {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("// META:") {
      break;
    }
    let rest = trimmed.trim_start_matches("// META:").trim();
    let Some((key, value)) = rest.split_once('=') else {
      continue;
    };
    if key.trim() != "script" {
      continue;
    }
    scripts.push(value.trim().to_string());
  }
  scripts
}

fn extract_html_refs(source: &str) -> Vec<String> {
  let mut out = Vec::new();

  // Any `src=...` attribute. This is a best-effort scan; the curated corpus should keep HTML
  // simple and deterministic.
  let src_attr_quoted =
    Regex::new(r#"(?i)\ssrc\s*=\s*["'](?P<url>[^"'>]+)["']"#).unwrap();
  let src_attr_unquoted = Regex::new(r#"(?i)\ssrc\s*=\s*(?P<url>[^\s"'>]+)"#).unwrap();
  for caps in src_attr_quoted.captures_iter(source) {
    if let Some(url) = caps.name("url").map(|m| m.as_str()) {
      out.push(url.to_string());
    }
  }
  for caps in src_attr_unquoted.captures_iter(source) {
    if let Some(url) = caps.name("url").map(|m| m.as_str()) {
      out.push(url.to_string());
    }
  }

  // `<link href=...>` only when `rel` implies a fetch.
  let link_tag = Regex::new(r#"(?is)<link\b[^>]*>"#).unwrap();
  let rel_attr =
    Regex::new(r#"(?is)(?:^|\s)rel\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap();
  let href_attr =
    Regex::new(r#"(?is)(?:^|\s)href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap();
  for m in link_tag.find_iter(source) {
    let tag = m.as_str();
    let Some(rel_caps) = rel_attr.captures(tag) else {
      continue;
    };
    let rel = rel_caps
      .get(1)
      .or_else(|| rel_caps.get(2))
      .or_else(|| rel_caps.get(3))
      .map(|m| m.as_str())
      .unwrap_or("");
    if !link_rel_requires_fetch(rel) {
      continue;
    }
    let Some(href_caps) = href_attr.captures(tag) else {
      continue;
    };
    let href = href_caps
      .get(1)
      .or_else(|| href_caps.get(2))
      .or_else(|| href_caps.get(3))
      .map(|m| m.as_str())
      .unwrap_or("");
    out.push(href.to_string());
  }

  out
}

fn extract_css_refs(source: &str) -> Vec<String> {
  let mut out = Vec::new();
  let css_url = Regex::new(r#"(?i)url\(\s*["']?(?P<url>[^"')]+)["']?\s*\)"#).unwrap();
  let css_import = Regex::new(r#"(?i)@import\s+["'](?P<url>[^"']+)["']"#).unwrap();
  for caps in css_url.captures_iter(source) {
    if let Some(url) = caps.name("url").map(|m| m.as_str()) {
      out.push(url.to_string());
    }
  }
  for caps in css_import.captures_iter(source) {
    if let Some(url) = caps.name("url").map(|m| m.as_str()) {
      out.push(url.to_string());
    }
  }
  out
}

fn file_url_path(tests_root: &Path, resources_root: &Path, path: &Path) -> Option<String> {
  if let Ok(rel) = path.strip_prefix(tests_root) {
    let rel = rel.to_string_lossy().replace('\\', "/");
    return Some(format!("/{rel}"));
  }
  if let Ok(rel) = path.strip_prefix(resources_root) {
    let rel = rel.to_string_lossy().replace('\\', "/");
    return Some(format!("/resources/{rel}"));
  }
  None
}

fn resolve_to_fs_path(tests_root: &Path, resources_root: &Path, resolved_url_path: &str) -> PathBuf {
  if let Some(rest) = resolved_url_path.strip_prefix("/resources/") {
    resources_root.join(rest)
  } else {
    tests_root.join(resolved_url_path.trim_start_matches('/'))
  }
}

#[test]
fn offline_corpus_is_valid() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let resources_root = resources_root();

  assert!(
    tests_root.is_dir() && resources_root.is_dir(),
    "missing expected corpus dirs under {}",
    corpus_root.display()
  );

  let mut errors: Vec<String> = Vec::new();
  let mut direct_report_offenders: Vec<String> = Vec::new();

  for entry in WalkDir::new(&corpus_root) {
    let entry = entry.expect("walkdir entry");
    if !entry.file_type().is_file() {
      continue;
    }
    let path = entry.path();
    if !is_text_file(path) {
      continue;
    }

    let Some(base_url_path) = file_url_path(&tests_root, &resources_root, path) else {
      continue;
    };

    let source = read_text(path);

    // After harness migration, corpus tests must use `testharness.js` and the reporter script
    // under `tests/wpt_dom/resources/`. Direct calls from test files bypass subtest collection and
    // diverge from standard WPT semantics. We intentionally allow these strings in `resources/`
    // because the harness plumbing lives there.
    if path.strip_prefix(&tests_root).is_ok()
      && (source.contains("__fastrender_wpt_report(")
        || source.contains("__fastrender_wpt_report_json"))
    {
      let rel = path
        .strip_prefix(&corpus_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
      direct_report_offenders.push(rel);
    }

    let mut refs = Vec::new();

    let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    if file_name.ends_with(".js") {
      refs.extend(extract_js_meta_scripts(&source));
    }
    if file_name.ends_with(".css") {
      refs.extend(extract_css_refs(&source));
    }
    if file_name.ends_with(".html") || file_name.ends_with(".htm") {
      refs.extend(extract_html_refs(&source));
    }

    for raw in refs {
      if is_ignored_url(&raw) {
        continue;
      }
      if is_network_url(&raw) {
        errors.push(format!(
          "network URL in {} ({}): {raw}",
          base_url_path,
          path.display()
        ));
        continue;
      }

      let (path_part, _suffix) = split_path_and_suffix(raw.trim());
      if path_part.is_empty() {
        continue;
      }

      let resolved = if path_part.starts_with('/') {
        normalize_url_path(path_part)
      } else {
        resolve_relative_url_path(&base_url_path, path_part)
      };

      let fs_path = resolve_to_fs_path(&tests_root, &resources_root, &resolved);
      if !fs_path.is_file() {
        errors.push(format!(
          "missing referenced file in {} ({}): {raw} -> {resolved} -> {}",
          base_url_path,
          path.display(),
          fs_path.display()
        ));
      }
    }
  }

  direct_report_offenders.sort();
  direct_report_offenders.dedup();
  if !direct_report_offenders.is_empty() {
    let mut msg = String::new();
    msg.push_str("corpus test files must not call `__fastrender_wpt_report*` directly ");
    msg.push_str("(use `testharness.js` + the reporter script under `resources/` instead)\n");
    msg.push_str("offending files:\n");
    for rel in &direct_report_offenders {
      msg.push_str("    - ");
      msg.push_str(rel);
      msg.push('\n');
    }
    errors.push(msg.trim_end().to_string());
  }

  errors.sort();
  errors.dedup();

  if !errors.is_empty() {
    let mut msg = String::new();
    msg.push_str("WPT DOM corpus validation failed:\n");
    for e in errors.iter().take(50) {
      msg.push_str("  - ");
      msg.push_str(e);
      msg.push('\n');
    }
    if errors.len() > 50 {
      msg.push_str(&format!("  ... and {} more\n", errors.len() - 50));
    }
    panic!("{msg}");
  }
}
