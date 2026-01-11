use fastrender::dom::{DomNode, DomNodeType};
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{FastRender, FastRenderConfig, RenderOptions};

fn find_element_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if let DomNodeType::Element { attributes, .. } = &node.node_type {
    if attributes
      .iter()
      .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == id)
    {
      return Some(node);
    }
  }
  node
    .children
    .iter()
    .find_map(|child| find_element_by_id(child, id))
}

fn collect_text(fragment: &fastrender::FragmentNode, texts: &mut Vec<String>) {
  if let FragmentContent::Text { text, .. } = &fragment.content {
    texts.push(text.to_string());
  }
  for child in fragment.children.iter() {
    collect_text(child, texts);
  }
}

#[test]
fn render_pipeline_ignores_noscript_when_scripting_enabled() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <noscript><div id="fallback">noscript text</div></noscript>
      </head>
      <body>
        <div id="live">live text</div>
      </body>
    </html>
  "#;

  // `prepare_html` exercises the main render pipeline, which should parse with
  // scripting-enabled semantics (mirroring Chrome baselines that block script execution via CSP).
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(200, 100))
    .expect("prepare");

  assert!(
    find_element_by_id(prepared.dom(), "fallback").is_none(),
    "expected <noscript> content in <head> to be ignored by the render pipeline"
  );
  assert!(
    find_element_by_id(prepared.dom(), "live").is_some(),
    "expected normal content to remain in the DOM"
  );

  let mut texts = Vec::new();
  collect_text(&prepared.fragment_tree().root, &mut texts);
  assert!(
    !texts.iter().any(|t| t.contains("noscript text")),
    "expected no noscript text fragments"
  );
  assert!(
    texts.iter().any(|t| t.contains("live text")),
    "expected normal text fragments"
  );
}

#[test]
fn render_pipeline_renders_noscript_when_scripting_disabled() {
  let config = FastRenderConfig::new().with_render_parse_scripting_enabled(false);
  let mut renderer = FastRender::with_config(config).expect("renderer");
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <noscript><div id="fallback-head">noscript head</div></noscript>
      </head>
      <body>
        <noscript><div id="fallback-body">noscript body</div></noscript>
        <div id="live">live text</div>
      </body>
    </html>
  "#;

  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(200, 100))
    .expect("prepare");

  assert!(
    find_element_by_id(prepared.dom(), "fallback-head").is_some(),
    "expected <noscript> content in <head> to be parsed when scripting is disabled"
  );
  assert!(
    find_element_by_id(prepared.dom(), "fallback-body").is_some(),
    "expected <noscript> content in <body> to be parsed when scripting is disabled"
  );
  assert!(
    find_element_by_id(prepared.dom(), "live").is_some(),
    "expected normal content to remain in the DOM"
  );

  let mut texts = Vec::new();
  collect_text(&prepared.fragment_tree().root, &mut texts);
  assert!(
    texts.iter().any(|t| t.contains("noscript head")),
    "expected noscript head text fragments; got: {texts:?}"
  );
  assert!(
    texts.iter().any(|t| t.contains("noscript body")),
    "expected noscript body text fragments; got: {texts:?}"
  );
  assert!(
    texts.iter().any(|t| t.contains("live text")),
    "expected normal text fragments; got: {texts:?}"
  );
}
