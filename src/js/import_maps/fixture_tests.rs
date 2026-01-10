use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use url::Url;

use super::create_import_map_parse_result;

fn read_fixture(relative_path: &str) -> String {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative_path);
  fs::read_to_string(&path).unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()))
}

fn extract_importmap_script_texts(html: &str) -> Vec<String> {
  static RE: OnceLock<Regex> = OnceLock::new();
  let re = RE.get_or_init(|| {
    Regex::new(r#"(?is)<script\b[^>]*\btype\s*=\s*(?:"importmap"|'importmap'|importmap)[^>]*>(.*?)</script>"#)
      .expect("importmap <script> regex compiles")
  });

  re.captures_iter(html)
    .map(|caps| caps[1].trim().to_string())
    .collect()
}

fn parse_all_importmap_scripts(html: &str, base_url: &Url) -> Vec<super::ImportMap> {
  let scripts = extract_importmap_script_texts(html);
  assert!(
    !scripts.is_empty(),
    "expected at least one <script type=\"importmap\"> in fixture"
  );

  scripts
    .into_iter()
    .map(|script_text| {
      let result = create_import_map_parse_result(&script_text, base_url);
      assert!(
        result.error_to_rethrow.is_none(),
        "import map parse error: {:?}",
        result.error_to_rethrow
      );
      result
        .import_map
        .expect("import map should be present when error_to_rethrow is None")
    })
    .collect()
}

#[test]
fn parses_importmap_from_techcrunch_fixture() {
  let html = read_fixture("tests/pages/fixtures/techcrunch.com/index.html");
  let base_url = Url::parse("https://techcrunch.com/").unwrap();
  let import_maps = parse_all_importmap_scripts(&html, &base_url);
  assert!(
    import_maps
      .iter()
      .any(|map| map.imports.contains_key("@wordpress/interactivity")),
    "expected @wordpress/interactivity entry in imports"
  );
}

#[test]
fn parses_importmap_from_msnbc_fixture() {
  let html = read_fixture("tests/pages/fixtures/msnbc.com/index.html");
  let base_url = Url::parse("https://www.ms.now/").unwrap();
  let import_maps = parse_all_importmap_scripts(&html, &base_url);
  assert!(
    import_maps
      .iter()
      .any(|map| map.imports.contains_key("@wordpress/interactivity")),
    "expected @wordpress/interactivity entry in imports"
  );
}

#[test]
fn parses_importmap_from_bing_fixture() {
  let html = read_fixture("tests/pages/fixtures/bing.com/index.html");
  let base_url = Url::parse("https://www.bing.com/").unwrap();
  let import_maps = parse_all_importmap_scripts(&html, &base_url);
  assert!(
    import_maps
      .iter()
      .any(|map| map.imports.contains_key("rms-answers-HomepageVNext-PeregrineWidgets")),
    "expected rms-answers-HomepageVNext-PeregrineWidgets entry in imports"
  );
}

#[test]
fn parses_importmap_from_yelp_fixture() {
  let html = read_fixture("tests/pages/fixtures/yelp.com/index.html");
  let base_url = Url::parse("https://www.yelp.com/").unwrap();
  let import_maps = parse_all_importmap_scripts(&html, &base_url);

  let Some(map) = import_maps.iter().find(|map| map.imports.contains_key("react")) else {
    panic!("expected react entry in imports");
  };

  let react_url = map
    .imports
    .get("react")
    .and_then(|addr| addr.as_ref())
    .expect("react address should be a valid URL")
    .as_str();
  let react_dom_url = map
    .imports
    .get("react-dom")
    .and_then(|addr| addr.as_ref())
    .expect("react-dom address should be a valid URL")
    .as_str();

  assert!(
    map.integrity.contains_key(react_url),
    "expected integrity entry for react URL {react_url}"
  );
  assert!(
    map.integrity.contains_key(react_dom_url),
    "expected integrity entry for react-dom URL {react_dom_url}"
  );
}

