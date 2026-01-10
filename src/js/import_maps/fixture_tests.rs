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

    // Normalize whitespace and case so we can match `type="importmap"` and `type=importmap`.
    let open_tag_normalized: String = open_tag
      .chars()
      .filter(|c| !c.is_ascii_whitespace())
      .collect::<String>()
      .to_ascii_lowercase();

    let content_start = open_tag_end + 1;
    let Some(close_tag_rel) = html[content_start..].find("</script>") else {
      panic!("missing </script> closing tag for <script> starting at offset {script_start}");
    };
    let close_tag_start = content_start + close_tag_rel;

    if open_tag_normalized.contains("type=\"importmap\"")
      || open_tag_normalized.contains("type='importmap'")
      || open_tag_normalized.contains("type=importmap")
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
fn fixture_import_map_parses_techcrunch() {
  let map = parse_fixture_import_map(
    "tests/pages/fixtures/techcrunch.com/index.html",
    "https://techcrunch.com/",
  );

  let url = expect_import_url(&map, "@wordpress/interactivity");
  assert!(
    url.as_str().starts_with("https://techcrunch.com/"),
    "expected @wordpress/interactivity to map to a https://techcrunch.com/ URL, got {url}"
  );
}

#[test]
fn fixture_import_map_parses_msnbc() {
  let map = parse_fixture_import_map("tests/pages/fixtures/msnbc.com/index.html", "https://www.ms.now/");

  let url = expect_import_url(&map, "@wordpress/interactivity");
  assert!(
    url.as_str().starts_with("https://www.ms.now/"),
    "expected @wordpress/interactivity to map to a https://www.ms.now/ URL, got {url}"
  );
}

#[test]
fn fixture_import_map_parses_bing() {
  let map = parse_fixture_import_map("tests/pages/fixtures/bing.com/index.html", "https://www.bing.com/");

  let url = expect_import_url(&map, "rms-answers-HomepageVNext-PeregrineWidgets");
  assert_eq!(
    url.scheme(),
    "https",
    "expected rms-answers-HomepageVNext-PeregrineWidgets to map to an absolute https URL, got {url}"
  );
  assert!(
    url.as_str().starts_with("https://assets.msn.com/"),
    "expected rms-answers-HomepageVNext-PeregrineWidgets to map to assets.msn.com (stable host), got {url}"
  );
}

#[test]
fn fixture_import_map_parses_yelp() {
  let map = parse_fixture_import_map("tests/pages/fixtures/yelp.com/index.html", "https://www.yelp.com/");

  for specifier in ["react", "react-dom"] {
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
  }
}

