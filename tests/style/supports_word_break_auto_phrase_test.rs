use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::WordBreak;

fn find_first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_first(child, tag) {
      return Some(found);
    }
  }
  None
}

#[test]
fn supports_word_break_auto_phrase_matches_in_supports_rule() {
  let dom = dom::parse_html(r#"<p lang="ja">text</p>"#).unwrap();
  let css = r#"
    p { word-break: normal; }
    @supports (word-break: auto-phrase) {
      :lang(ja) { word-break: auto-phrase; }
    }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let p = find_first(&styled, "p").expect("p");
  assert!(
    matches!(p.styles.word_break, WordBreak::AutoPhrase),
    "expected @supports-gated `word-break:auto-phrase` to apply"
  );
}

