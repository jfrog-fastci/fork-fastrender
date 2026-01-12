use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn find_first_block_with_line_children<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) = find_first_block_with_line_children(child) {
      return Some(found);
    }
  }
  None
}

fn line_texts(html: &str) -> Vec<String> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let block = find_first_block_with_line_children(&fragments.root)
    .expect("expected a block fragment with line children");

  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| {
      let mut text = String::new();
      collect_text(line, &mut text);
      text
    })
    .collect()
}

#[test]
fn word_break_auto_phrase_changes_line_breaking_for_japanese_text() {
  let base = r#"
    <style>
      p {
        width: 160px;
        margin: 0;
        padding: 0;
        font-family: "Noto Sans JP";
        font-size: 20px;
        line-height: 1;
        white-space: normal;
      }
    </style>
  "#;

  let normal_html = format!(
    r#"{base}
      <p lang="ja" style="word-break: normal">おじいさんとおばあさん</p>
    "#
  );
  let auto_html = format!(
    r#"{base}
      <p lang="ja" style="word-break: auto-phrase">おじいさんとおばあさん</p>
    "#
  );

  let normal_lines = line_texts(&normal_html);
  let auto_lines = line_texts(&auto_html);

  assert_ne!(
    normal_lines, auto_lines,
    "auto-phrase should affect Japanese line breaking compared to normal"
  );
  assert_eq!(auto_lines, ["おじいさんと", "おばあさん"]);
}

#[test]
fn word_break_auto_phrase_gitlab_supports_gate_enables_rule() {
  let html = r#"
    <style>
      p {
        width: 160px;
        margin: 0;
        padding: 0;
        font-family: "Noto Sans JP";
        font-size: 20px;
        line-height: 1;
        white-space: normal;
        word-break: normal;
      }

      /* GitLab-style gating: apply `word-break:auto-phrase` only when supported. */
      @supports (word-break: auto-phrase) {
        :lang(ja) { word-break: auto-phrase; }
      }
    </style>
    <p lang="ja">おじいさんとおばあさん</p>
  "#;

  let lines = line_texts(html);
  assert_eq!(lines, ["おじいさんと", "おばあさん"]);
}
