use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade::{
  apply_styles_with_media, apply_styles_with_media_and_options, CascadeOptions, StyledNode,
};
use fastrender::style::media::MediaContext;
use fastrender::Rgba;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn document_rules_do_not_leak_into_shadow_trees_without_stylesheets() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <span id="shadow">X</span>
      </template>
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let media = MediaContext::screen(800.0, 600.0);

  let document_sheet =
    parse_stylesheet("span { color: rgb(255, 0, 0); }").expect("parse stylesheet");

  // Spec behavior: document-scoped author rules do not apply inside shadow trees, even when the
  // shadow tree has no author stylesheets.
  let styled_spec = apply_styles_with_media(&dom, &document_sheet, &media);
  let spec_color = find_by_id(&styled_spec, "shadow")
    .expect("shadow span")
    .styles
    .color;

  // Compare against a baseline cascade with an empty document stylesheet to avoid depending on a
  // specific UA default color.
  let baseline_sheet = StyleSheet { rules: Vec::new() };
  let styled_baseline = apply_styles_with_media(&dom, &baseline_sheet, &media);
  let baseline_color = find_by_id(&styled_baseline, "shadow")
    .expect("shadow span baseline")
    .styles
    .color;

  assert_eq!(
    spec_color, baseline_color,
    "document stylesheet must not affect styles inside a shadow tree without author styles"
  );
  assert_ne!(
    spec_color,
    Rgba::rgb(255, 0, 0),
    "sanity check: spec-mode shadow styles should not use the document rule's red"
  );

  // Legacy compatibility mode: document rules are applied inside shadow trees.
  let styled_legacy = apply_styles_with_media_and_options(
    &dom,
    &document_sheet,
    &media,
    CascadeOptions::legacy_shadow_document_fallback(),
  );
  let legacy_color = find_by_id(&styled_legacy, "shadow")
    .expect("shadow span legacy")
    .styles
    .color;
  assert_eq!(
    legacy_color,
    Rgba::rgb(255, 0, 0),
    "legacy mode should apply the document rule inside the shadow tree"
  );
}
