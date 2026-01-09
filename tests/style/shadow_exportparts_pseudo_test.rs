use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::parse_html;
use fastrender::style::cascade::apply_style_set_with_media_target_and_imports;
use fastrender::style::content::{ContentItem, ContentValue};
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use fastrender::Rgba;
use std::collections::HashMap;

fn find_by_id<'a>(
  node: &'a fastrender::style::cascade::StyledNode,
  id: &str,
) -> Option<&'a fastrender::style::cascade::StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn apply_styles(html: &str, css: &str) -> fastrender::style::cascade::StyledNode {
  let dom = parse_html(html).expect("parsed html");
  let stylesheet = parse_stylesheet(css).expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  )
}

#[test]
fn exportparts_pseudo_forwarding_allows_part_selector_to_style_before() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <p id="p" exportparts="::before: preceding-text">Main</p>
      </template>
    </x-host>
  "#;

  let styled = apply_styles(
    html,
    r#"
      x-host::part(preceding-text) {
        content: "X";
        color: rgb(1, 2, 3);
      }
    "#,
  );
  let p = find_by_id(&styled, "p").expect("shadow element");
  let before = p.before_styles.as_ref().expect("generated ::before");

  assert_eq!(before.color, Rgba::rgb(1, 2, 3));
  assert_eq!(
    before.content_value,
    ContentValue::Items(vec![ContentItem::String("X".to_string())])
  );
}

#[test]
fn exportparts_pseudo_forwarding_allows_part_selector_to_style_file_selector_button() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <input id="file" type="file" exportparts="::file-selector-button: upload-button">
      </template>
    </x-host>
  "#;

  let styled = apply_styles(
    html,
    r#"
      x-host::part(upload-button) {
        color: rgb(1, 2, 3);
      }
    "#,
  );
  let input = find_by_id(&styled, "file").expect("shadow input");
  let button = input
    .file_selector_button_styles
    .as_ref()
    .expect("generated ::file-selector-button");

  assert_eq!(button.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn exportparts_pseudo_forwarding_allows_part_selector_to_style_slider_thumb() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <input id="range" type="range" exportparts="::slider-thumb: thumb">
      </template>
    </x-host>
  "#;

  let styled = apply_styles(
    html,
    r#"
      x-host::part(thumb) {
        color: rgb(1, 2, 3);
      }
    "#,
  );
  let input = find_by_id(&styled, "range").expect("shadow input");
  let thumb = input
    .slider_thumb_styles
    .as_ref()
    .expect("generated ::slider-thumb");
  assert_eq!(thumb.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn exportparts_pseudo_forwarding_does_not_forward_marker() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <ul>
          <li id="li" style="color: rgb(9, 9, 9)" exportparts="::marker: bullet">Item</li>
        </ul>
      </template>
    </x-host>
  "#;

  let styled = apply_styles(
    html,
    r#"
      x-host::part(bullet) {
        content: "X";
        color: rgb(1, 2, 3);
      }
    "#,
  );
  let li = find_by_id(&styled, "li").expect("shadow list item");
  let marker = li.marker_styles.as_ref().expect("generated ::marker");

  assert_eq!(marker.color, Rgba::rgb(9, 9, 9));
  assert_ne!(
    marker.content_value,
    ContentValue::Items(vec![ContentItem::String("X".to_string())])
  );
}
