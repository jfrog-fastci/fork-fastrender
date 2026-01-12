use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{FastRender, FastRenderConfig};

fn collect_text(fragment: &fastrender::FragmentNode, texts: &mut Vec<String>) {
  if let FragmentContent::Text { text, .. } = &fragment.content {
    texts.push(text.to_string());
  }
  for child in fragment.children.iter() {
    collect_text(child, texts);
  }
}

#[test]
fn noscript_display_rule_is_ignored_when_scripting_enabled() {
  let config = FastRenderConfig::new().with_dom_scripting_enabled(true);
  let mut renderer = FastRender::with_config(config).expect("renderer");
  let dom = renderer
    .parse_html(
      r#"
        <html>
          <head>
            <style>
              /* Even if the author attempts to force noscript visible, JS-enabled parsing should
                 suppress it. */
              noscript { display: block !important; }
            </style>
          </head>
          <body>
            <noscript><div>noscript text</div></noscript>
            <div>live text</div>
          </body>
        </html>
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
