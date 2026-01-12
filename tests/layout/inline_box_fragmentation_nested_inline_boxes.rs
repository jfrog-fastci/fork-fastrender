use std::sync::Arc;

use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::inline::InlineFormattingContext;
use fastrender::style::display::FormattingContextType;
use fastrender::text::font_db::FontConfig;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext};

fn collect_text(fragment: &FragmentNode, out: &mut String) {
  match &fragment.content {
    FragmentContent::Text { text, .. } => out.push_str(text),
    _ => {
      for child in fragment.children.iter() {
        collect_text(child, out);
      }
    }
  }
}

#[test]
fn inline_box_fragmentation_splits_nested_inline_boxes() {
  // Regression test for inline box fragmentation when an inline box contains multiple nested
  // inline boxes and the later child does not fit entirely in the remaining line width.
  //
  // Real-world pages (e.g. vogue.com headlines) commonly wrap a leading word in an inline element
  // like `<em>` with no whitespace separating it from the subsequent text. Inline layout should
  // still break at spaces *inside* the subsequent text, rather than forcing a line break at the
  // inline-element boundary.

  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let ifc = InlineFormattingContext::with_font_context(font_context);

  let mut style = ComputedStyle::default();
  style.font_family = vec!["Noto Sans Mono".to_string()].into();
  style.font_size = 20.0;
  let style = Arc::new(style);

  // <span><em>Vogue</em>’s 55 Best Dressed People of 2025</span>
  let mut em_style = (*style).clone();
  em_style.font_style = fastrender::style::types::FontStyle::Italic;
  let em_style = Arc::new(em_style);

  let em = BoxNode::new_inline(
    em_style.clone(),
    vec![BoxNode::new_text(em_style, "Vogue".to_string())],
  );

  // Model the post-`</em>` text as an anonymous inline wrapper, matching the box tree patterns
  // produced by our HTML/CSS box generation.
  let post_em = BoxNode::new_anonymous_inline(
    style.clone(),
    vec![BoxNode::new_text(
      style.clone(),
      "’s 55 Best Dressed People of 2025".to_string(),
    )],
  );

  let mut span = BoxNode::new_inline(style.clone(), vec![em, post_em]);
  span.id = 1;

  let root = BoxNode::new_block(style, FormattingContextType::Inline, vec![span]);

  // The line width is chosen so the full string requires wrapping, but there is enough remaining
  // width after the first child (`<em>Vogue</em>`) to include at least the "’s" prefix on the first
  // line when nested inline boxes are split correctly.
  let fragment = ifc
    .layout(&root, &LayoutConstraints::definite(140.0, 200.0))
    .expect("inline layout");

  let lines: Vec<&FragmentNode> = fragment
    .iter_fragments()
    .filter(|f| matches!(f.content, FragmentContent::Line { .. }))
    .collect();

  assert!(
    lines.len() >= 2,
    "expected wrapped inline content to produce multiple line fragments, got {}",
    lines.len()
  );

  let mut first_line_text = String::new();
  collect_text(lines[0], &mut first_line_text);

  assert!(
    first_line_text.contains("’s"),
    "expected first line to include text from the nested inline box (got {:?})",
    first_line_text
  );
}
