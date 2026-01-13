use crate::api::FastRender;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaType;
use crate::style::types::{TextAutospace, TextSpacingTrim};

fn styled_tree_for(html: &str) -> StyledNode {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed dom");
  renderer
    .layout_document_for_media_intermediates(&dom, 800, 600, MediaType::Screen)
    .expect("laid out")
    .styled_tree
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .map(|value| value == id)
    .unwrap_or(false)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn text_spacing_trim_parses_keywords_and_inherits() {
  let html = r#"
    <style>
      #trim_auto { text-spacing-trim: auto; }
      #trim_normal { text-spacing-trim: normal; }
      #trim_space_all { text-spacing-trim: space-all; }
      #trim_space_first { text-spacing-trim: space-first; }
      #trim_start { text-spacing-trim: trim-start; }
      #trim_both { text-spacing-trim: trim-both; }
      #trim_all { text-spacing-trim: trim-all; }
      #trim_invalid { text-spacing-trim: nope; }
    </style>
    <div id="trim_auto"></div>
    <div id="trim_normal"></div>
    <div id="trim_space_all"></div>
    <div id="trim_space_first"></div>
    <div id="trim_start"></div>
    <div id="trim_both"></div>
    <div id="trim_all"></div>
    <div id="trim_invalid"></div>
    <div id="inherit_parent" style="text-spacing-trim: trim-start">
      <span id="inherit_child"></span>
    </div>
  "#;

  let styled = styled_tree_for(html);

  let get = |id| find_by_id(&styled, id).unwrap_or_else(|| panic!("missing #{id}"));

  assert_eq!(get("trim_auto").styles.text_spacing_trim, TextSpacingTrim::Auto);
  assert_eq!(get("trim_normal").styles.text_spacing_trim, TextSpacingTrim::Normal);
  assert_eq!(
    get("trim_space_all").styles.text_spacing_trim,
    TextSpacingTrim::SpaceAll
  );
  assert_eq!(
    get("trim_space_first").styles.text_spacing_trim,
    TextSpacingTrim::SpaceFirst
  );
  assert_eq!(get("trim_start").styles.text_spacing_trim, TextSpacingTrim::TrimStart);
  assert_eq!(get("trim_both").styles.text_spacing_trim, TextSpacingTrim::TrimBoth);
  assert_eq!(get("trim_all").styles.text_spacing_trim, TextSpacingTrim::TrimAll);

  // Invalid values are ignored, leaving the initial value.
  assert_eq!(
    get("trim_invalid").styles.text_spacing_trim,
    TextSpacingTrim::Normal
  );

  // The property is inherited.
  assert_eq!(
    get("inherit_child").styles.text_spacing_trim,
    TextSpacingTrim::TrimStart
  );
}

#[test]
fn text_spacing_shorthand_expands_to_longhands() {
  let html = r#"
    <style>
      #none { text-spacing: none; }
      #auto { text-spacing: auto; }
      #combo { text-spacing: trim-both no-autospace; }
      #invalid { text-spacing: trim-start trim-both; }
    </style>
    <div id="none"></div>
    <div id="auto"></div>
    <div id="combo"></div>
    <div id="invalid"></div>
  "#;

  let styled = styled_tree_for(html);
  let get = |id| find_by_id(&styled, id).unwrap_or_else(|| panic!("missing #{id}"));

  // Spec: `text-spacing: none` expands to `text-spacing-trim: space-all` and
  // `text-autospace: no-autospace`.
  assert_eq!(get("none").styles.text_spacing_trim, TextSpacingTrim::SpaceAll);
  assert_eq!(get("none").styles.text_autospace, TextAutospace::NoAutospace);

  // Spec: `text-spacing: auto` sets both longhands to `auto`.
  assert_eq!(get("auto").styles.text_spacing_trim, TextSpacingTrim::Auto);
  assert_eq!(get("auto").styles.text_autospace, TextAutospace::Auto);

  assert_eq!(get("combo").styles.text_spacing_trim, TextSpacingTrim::TrimBoth);
  assert_eq!(get("combo").styles.text_autospace, TextAutospace::NoAutospace);

  // Invalid shorthand leaves both longhands at their initial values.
  assert_eq!(
    get("invalid").styles.text_spacing_trim,
    TextSpacingTrim::Normal
  );
  assert_eq!(get("invalid").styles.text_autospace, TextAutospace::Normal);
}

