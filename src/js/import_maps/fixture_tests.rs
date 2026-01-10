use super::parse_import_map_string;

use std::path::Path;

use url::Url;

fn fixture_path(relative: &str) -> std::path::PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read_fixture(relative: &str) -> String {
  let path = fixture_path(relative);
  std::fs::read_to_string(&path).unwrap_or_else(|err| {
    panic!(
      "failed to read fixture {}: {err}",
      path.to_string_lossy()
    )
  })
}

fn extract_html_attr_value(tag: &str, attr_name: &str) -> Option<String> {
  let bytes = tag.as_bytes();
  let mut i = 0usize;
  let len = bytes.len();
  let attr_name = attr_name.to_ascii_lowercase();

  // Skip leading `<script` (or any leading tag name). This helper is only used by the import map
  // tests, so we keep it intentionally lightweight and limited.
  if let Some(start) = tag.find('<') {
    i = start + 1;
    while i < len && bytes[i].is_ascii_alphabetic() {
      i += 1;
    }
  }

  while i < len {
    while i < len && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= len || bytes[i] == b'>' {
      break;
    }

    // Parse attribute name.
    let name_start = i;
    while i < len
      && !bytes[i].is_ascii_whitespace()
      && bytes[i] != b'='
      && bytes[i] != b'>'
      && bytes[i] != b'/'
    {
      i += 1;
    }
    if name_start == i {
      i += 1;
      continue;
    }
    let name = tag[name_start..i].to_ascii_lowercase();

    while i < len && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value = String::new();
    if i < len && bytes[i] == b'=' {
      i += 1;
      while i < len && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      if i < len {
        match bytes[i] {
          b'"' | b'\'' => {
            let quote = bytes[i];
            i += 1;
            let value_start = i;
            while i < len && bytes[i] != quote {
              i += 1;
            }
            value = tag[value_start..i].to_string();
            if i < len && bytes[i] == quote {
              i += 1;
            }
          }
          _ => {
            let value_start = i;
            while i < len && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
              i += 1;
            }
            value = tag[value_start..i].to_string();
          }
        }
      }
    }

    if name == attr_name {
      return Some(value);
    }
  }

  None
}

fn extract_first_importmap_script_json(html: &str) -> String {
  let mut search_start = 0;
  loop {
    let Some(script_start_rel) = html[search_start..].find("<script") else {
      panic!("no <script> tag found after offset {search_start}");
    };
    let script_start = search_start + script_start_rel;

    let Some(open_tag_end_rel) = html[script_start..].find('>') else {
      panic!("unterminated <script ...> tag starting at offset {script_start}");
    };
    let open_tag_end = script_start + open_tag_end_rel;
    let open_tag = &html[script_start..=open_tag_end];

    let content_start = open_tag_end + 1;
    let Some(close_tag_rel) = html[content_start..].find("</script>") else {
      panic!("missing </script> closing tag for <script> starting at offset {script_start}");
    };
    let close_tag_start = content_start + close_tag_rel;

    if extract_html_attr_value(open_tag, "type").is_some_and(|value| value.eq_ignore_ascii_case("importmap"))
    {
      return html[content_start..close_tag_start].trim().to_string();
    }

    // Skip past this whole script element so we don't accidentally match nested `<script` inside
    // inline JS strings.
    search_start = close_tag_start + "</script>".len();
  }
}

fn parse_fixture_import_map(fixture_html: &str, base_url: &str) -> super::ImportMap {
  let html = read_fixture(fixture_html);
  let json = extract_first_importmap_script_json(&html);
  let base = Url::parse(base_url).unwrap_or_else(|err| panic!("invalid base URL {base_url:?}: {err}"));
  let (map, _warnings) = parse_import_map_string(&json, &base).unwrap_or_else(|err| {
    panic!("failed to parse import map extracted from {fixture_html}: {err:?}\nJSON: {json}")
  });
  map
}

fn expect_import_url<'a>(map: &'a super::ImportMap, key: &str) -> &'a Url {
  map
    .imports
    .get(key)
    .unwrap_or_else(|| panic!("expected imports entry for {key:?}"))
    .as_ref()
    .unwrap_or_else(|| panic!("expected {key:?} to map to a URL, got null"))
}

#[test]
fn extract_first_importmap_script_json_skips_importmap_shim_scripts() {
  let html = r#"
<!doctype html>
<script type="importmap-shim">{ "imports": { "a": "/a.js" } }</script>
<script type="importmap">{ "imports": { "b": "/b.js" } }</script>
"#;
  let json = extract_first_importmap_script_json(html);
  assert!(json.contains(r#""b""#), "unexpected import map JSON: {json}");
}

#[test]
fn fixture_import_map_parses_techcrunch() {
  let map = parse_fixture_import_map(
    "tests/pages/fixtures/techcrunch.com/index.html",
    "https://techcrunch.com/",
  );

  let url = expect_import_url(&map, "@wordpress/interactivity");
  assert!(
    url
      .as_str()
      .starts_with("https://techcrunch.com/wp-includes/js/dist/script-modules/interactivity/"),
    "unexpected @wordpress/interactivity URL: {url}"
  );
}

#[test]
fn fixture_import_map_parses_msnbc() {
  let map = parse_fixture_import_map("tests/pages/fixtures/msnbc.com/index.html", "https://www.ms.now/");

  let url = expect_import_url(&map, "@wordpress/interactivity");
  assert!(
    url
      .as_str()
      .starts_with("https://www.ms.now/wp-includes/js/dist/script-modules/interactivity/"),
    "unexpected @wordpress/interactivity URL: {url}"
  );
}

#[test]
fn fixture_import_map_parses_bing() {
  let map = parse_fixture_import_map("tests/pages/fixtures/bing.com/index.html", "https://www.bing.com/");

  let url = expect_import_url(&map, "rms-answers-HomepageVNext-PeregrineWidgets");
  assert_eq!(url.as_str(), "https://assets.msn.com/bundles/v1/bingHomepage/latest/widget-initializer.js");
}

#[test]
fn fixture_import_map_parses_yelp() {
  let map = parse_fixture_import_map("tests/pages/fixtures/yelp.com/index.html", "https://www.yelp.com/");

  for (specifier, expected_integrity) in [
    (
      "react",
      "sha384-UfZTcbQo0urRc9EDyVtaRtxuTnwNWsj3LZU1SOW0wRwXgoc1xPcLpkBisYVl842u",
    ),
    (
      "react-dom",
      "sha384-5NZxAm34SqlAowGqMPn47F6pkDLPYoGmdSnkCZa9IWUZR1kkFh1Yb3U8tTtaFDtl",
    ),
  ] {
    let url = expect_import_url(&map, specifier);
    assert_eq!(
      url.scheme(),
      "https",
      "expected {specifier} to map to an absolute https URL, got {url}"
    );
    assert!(
      url.as_str().ends_with(".mjs"),
      "expected {specifier} to map to a .mjs module URL, got {url}"
    );
    let integrity = map
      .integrity
      .get(url.as_str())
      .unwrap_or_else(|| panic!("expected integrity metadata for {specifier} ({url})"));
    assert_eq!(integrity, expected_integrity);
  }
}
