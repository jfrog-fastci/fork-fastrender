use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::ComputedStyle;
use std::sync::Arc;

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

fn div_styles(html: &str, css: &str) -> Arc<ComputedStyle> {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let div = find_first(&styled, "div").expect("div");
  Arc::clone(&div.styles)
}

#[test]
fn webkit_font_smoothing_antialiased_disables_subpixel_aa() {
  let styles = div_styles(
    "<div></div>",
    "div { -webkit-font-smoothing: antialiased; }",
  );
  assert!(
    !styles.allow_subpixel_aa,
    "expected -webkit-font-smoothing: antialiased to disable subpixel AA"
  );
}

#[test]
fn moz_osx_font_smoothing_grayscale_disables_subpixel_aa() {
  let styles = div_styles("<div></div>", "div { -moz-osx-font-smoothing: grayscale; }");
  assert!(
    !styles.allow_subpixel_aa,
    "expected -moz-osx-font-smoothing: grayscale to disable subpixel AA"
  );
}

#[test]
fn font_smoothing_inherits_when_unspecified() {
  let styles = div_styles(
    "<body><div></div></body>",
    "body { -webkit-font-smoothing: antialiased; }",
  );
  assert!(
    !styles.allow_subpixel_aa,
    "expected -webkit-font-smoothing to inherit to descendants"
  );
}

#[test]
fn font_smoothing_unset_behaves_like_inherit() {
  let styles = div_styles(
    "<body><div></div></body>",
    "body { -webkit-font-smoothing: antialiased; } div { -webkit-font-smoothing: unset; }",
  );
  assert!(
    !styles.allow_subpixel_aa,
    "expected -webkit-font-smoothing: unset to behave like inherit for inherited property"
  );
}

#[test]
fn font_smooth_never_disables_subpixel_aa() {
  let styles = div_styles("<div></div>", "div { font-smooth: never; }");
  assert!(
    !styles.allow_subpixel_aa,
    "expected font-smooth: never to disable subpixel AA"
  );
}

#[test]
fn font_smooth_auto_overrides_inherited_antialiased() {
  let styles = div_styles(
    "<body><div></div></body>",
    "body { -webkit-font-smoothing: antialiased; } div { font-smooth: auto; }",
  );
  assert!(
    styles.allow_subpixel_aa,
    "expected font-smooth: auto to override inherited -webkit-font-smoothing: antialiased and enable subpixel AA"
  );
}
