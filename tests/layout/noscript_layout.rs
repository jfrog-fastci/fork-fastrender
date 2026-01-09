use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::dom::DomParseOptions;
use fastrender::FastRender;

fn collect_text(fragment: &fastrender::FragmentNode, texts: &mut Vec<String>) {
  if let FragmentContent::Text { text, .. } = &fragment.content {
    texts.push(text.to_string());
  }
  for child in fragment.children.iter() {
    collect_text(child, texts);
  }
}

#[test]
fn noscript_content_is_rendered_when_scripting_disabled() {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = fastrender::dom::parse_html_with_options(
    r#"
      <html><body>
          <noscript><div>noscript text</div></noscript>
          <div>live text</div>
      </body></html>
  "#,
    DomParseOptions::with_scripting_enabled(false),
  )
  .expect("parse");

  let tree = renderer.layout_document(&dom, 200, 100).expect("layout");
  let mut texts = Vec::new();
  collect_text(&tree.root, &mut texts);

  assert!(
    texts.iter().any(|t| t.contains("noscript text")),
    "expected noscript text"
  );
  assert!(
    texts.iter().any(|t| t.contains("live text")),
    "expected normal text"
  );
}

#[test]
fn noscript_content_is_not_rendered_when_scripting_enabled() {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer
    .parse_html(
      r#"
        <html><body>
            <noscript><div>noscript text</div></noscript>
            <div>live text</div>
        </body></html>
    "#,
    )
    .expect("parse");

  let tree = renderer.layout_document(&dom, 200, 100).expect("layout");
  let mut texts = Vec::new();
  collect_text(&tree.root, &mut texts);

  assert!(
    !texts.iter().any(|t| t.contains("noscript text")),
    "did not expect noscript text when scripting is enabled"
  );
  assert!(
    texts.iter().any(|t| t.contains("live text")),
    "expected normal text"
  );
}

#[test]
fn noscript_and_scripting_media_queries_are_consistent() {
  // When FastRender parses HTML with scripting disabled, <noscript> should be parsed/rendered AND
  // MQ5 `(scripting: none)` should match so authors can style those fallbacks.
  let html = r#"
    <html>
      <head>
        <style>
          #noscript-only { display: none; }
          @media (scripting: none) { #noscript-only { display: block; } }
        </style>
      </head>
      <body>
        <noscript><div id="noscript-only">noscript mq text</div></noscript>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 100).expect("layout");
  let mut texts = Vec::new();
  collect_text(&tree.root, &mut texts);

  assert!(
    texts.iter().any(|t| t.contains("noscript mq text")),
    "expected <noscript> fallback styled by (scripting: none); got texts: {texts:?}"
  );
}
