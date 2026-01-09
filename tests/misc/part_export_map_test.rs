use std::collections::HashMap;

use fastrender::css::selectors::ExportedPartTarget;
use fastrender::css::selectors::PseudoElement;
use fastrender::dom::{compute_part_export_map_with_ids, enumerate_dom_ids, parse_html, DomNode};

fn find_dom_by_id<'a>(root: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  // `DomNode::walk_tree` uses a higher-ranked closure bound which makes it awkward to return a
  // borrowed node from the callback. Use an explicit stack so we can return `&'a DomNode`.
  let mut stack: Vec<&'a DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn node_id_by_html_id(
  root: &DomNode,
  ids: &HashMap<*const DomNode, usize>,
  html_id: &str,
) -> usize {
  let node = find_dom_by_id(root, html_id).unwrap_or_else(|| panic!("missing #{html_id}"));
  *ids
    .get(&(node as *const DomNode))
    .unwrap_or_else(|| panic!("missing dom id for #{html_id}"))
}

#[test]
fn part_export_map_direct_part_on_host_shadow_root() {
  let html = r#"
    <div id="outer-host">
      <template shadowrootmode="open">
        <div id="outer-part" part="outer"></div>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let outer_host_id = node_id_by_html_id(&dom, &ids, "outer-host");
  let outer_part_id = node_id_by_html_id(&dom, &ids, "outer-part");

  let exports = map
    .exports_for_host(outer_host_id)
    .expect("outer host exports");
  let targets = exports.get("outer").expect("outer part exports");
  assert!(
    targets.contains(&ExportedPartTarget::Element(outer_part_id)),
    "outer part exports must contain the part element"
  );
}

#[test]
fn part_export_map_nested_host_reexports_into_ancestor_host() {
  let html = r#"
    <div id="outer-host">
      <template shadowrootmode="open">
        <div id="inner-host" exportparts="inner:reexported">
          <template shadowrootmode="open">
            <span id="inner-part" part="inner"></span>
          </template>
        </div>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let outer_host_id = node_id_by_html_id(&dom, &ids, "outer-host");
  let inner_part_id = node_id_by_html_id(&dom, &ids, "inner-part");

  let exports = map
    .exports_for_host(outer_host_id)
    .expect("outer host exports");
  let targets = exports
    .get("reexported")
    .expect("reexported part exports");
  assert!(
    targets.contains(&ExportedPartTarget::Element(inner_part_id)),
    "reexported part exports must contain forwarded element target"
  );
}

#[test]
fn part_export_map_alias_chaining_across_multiple_levels() {
  let html = r#"
    <div id="outer-host" exportparts="mid:outer">
      <template shadowrootmode="open">
        <div id="inner-host" exportparts="inner:mid">
          <template shadowrootmode="open">
            <span id="inner-part" part="inner"></span>
          </template>
        </div>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let outer_host_id = node_id_by_html_id(&dom, &ids, "outer-host");
  let inner_part_id = node_id_by_html_id(&dom, &ids, "inner-part");

  let exports = map
    .exports_for_host(outer_host_id)
    .expect("outer host exports");
  let targets = exports.get("outer").expect("outer alias exports");
  assert!(
    targets.contains(&ExportedPartTarget::Element(inner_part_id)),
    "chained alias exports must include forwarded element target"
  );
  assert!(
    exports.get("mid").is_none(),
    "outer host exportparts mapping must not leak intermediate part name"
  );
}

#[test]
fn part_export_map_does_not_leak_nested_shadow_without_exportparts() {
  let html = r#"
    <div id="outer-host">
      <template shadowrootmode="open">
        <div id="inner-host">
          <template shadowrootmode="open">
            <span id="inner-part" part="inner"></span>
          </template>
        </div>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let outer_host_id = node_id_by_html_id(&dom, &ids, "outer-host");
  let inner_part_id = node_id_by_html_id(&dom, &ids, "inner-part");

  let exports = map
    .exports_for_host(outer_host_id)
    .expect("outer host exports");
  assert!(
    exports.values().all(|targets| {
      !targets.contains(&ExportedPartTarget::Element(inner_part_id))
    }),
    "outer host exports must not include inner shadow part without exportparts"
  );
}

#[test]
fn part_export_map_host_exportparts_hides_unlisted_parts() {
  let html = r#"
    <div id="host" exportparts="one">
      <template shadowrootmode="open">
        <span id="one-part" part="one"></span>
        <span id="two-part" part="two"></span>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let host_id = node_id_by_html_id(&dom, &ids, "host");
  let one_id = node_id_by_html_id(&dom, &ids, "one-part");

  let exports = map.exports_for_host(host_id).expect("host exports");
  let one_targets = exports.get("one").expect("one part exports");
  assert!(
    one_targets.contains(&ExportedPartTarget::Element(one_id)),
    "listed part should be exported"
  );
  assert!(
    exports.get("two").is_none(),
    "unlisted part must not be exported when exportparts is present"
  );
}

#[test]
fn part_export_map_host_exportparts_rename_hides_internal_name() {
  let html = r#"
    <div id="host" exportparts="inner:outer">
      <template shadowrootmode="open">
        <span id="part" part="inner"></span>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let host_id = node_id_by_html_id(&dom, &ids, "host");
  let part_id = node_id_by_html_id(&dom, &ids, "part");

  let exports = map.exports_for_host(host_id).expect("host exports");
  let targets = exports.get("outer").expect("outer part exports");
  assert!(
    targets.contains(&ExportedPartTarget::Element(part_id)),
    "renamed part must be exported under its alias"
  );
  assert!(
    exports.get("inner").is_none(),
    "internal part name must not be exported when exportparts renames it"
  );
}

#[test]
fn part_export_map_host_exportparts_does_not_chain_mappings() {
  let html = r#"
    <div id="host" exportparts="inner:mid, mid:outer">
      <template shadowrootmode="open">
        <span id="part" part="inner"></span>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let host_id = node_id_by_html_id(&dom, &ids, "host");
  let part_id = node_id_by_html_id(&dom, &ids, "part");

  let exports = map.exports_for_host(host_id).expect("host exports");
  let targets = exports.get("mid").expect("mid part exports");
  assert!(
    targets.contains(&ExportedPartTarget::Element(part_id)),
    "first mapping should export the part under its alias"
  );
  assert!(
    exports.get("outer").is_none(),
    "exportparts mappings must not chain within a single attribute value"
  );
}

#[test]
fn part_export_map_includes_exportparts_pseudo_elements() {
  let html = r#"
    <div id="host">
      <template shadowrootmode="open">
        <p id="p" exportparts="::before: preceding-text">Main</p>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let host_id = node_id_by_html_id(&dom, &ids, "host");
  let p_id = node_id_by_html_id(&dom, &ids, "p");

  let exports = map.exports_for_host(host_id).expect("host exports");
  let targets = exports
    .get("preceding-text")
    .expect("pseudo part exports");
  assert!(
    targets.contains(&ExportedPartTarget::Pseudo {
      node_id: p_id,
      pseudo: PseudoElement::Before,
    }),
    "exportparts must expose the element's ::before pseudo-element as a part target"
  );
}

#[test]
fn part_export_map_includes_exportparts_file_selector_button_pseudo_element() {
  let html = r#"
    <div id="host">
      <template shadowrootmode="open">
        <input id="file" type="file" exportparts="::file-selector-button: upload-button">
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let host_id = node_id_by_html_id(&dom, &ids, "host");
  let file_id = node_id_by_html_id(&dom, &ids, "file");

  let exports = map.exports_for_host(host_id).expect("host exports");
  let targets = exports
    .get("upload-button")
    .expect("file-selector-button exports");
  assert!(
    targets.contains(&ExportedPartTarget::Pseudo {
      node_id: file_id,
      pseudo: PseudoElement::FileSelectorButton,
    }),
    "exportparts must expose the element's ::file-selector-button pseudo-element as a part target"
  );
}

#[test]
fn part_export_map_includes_exportparts_slider_thumb_pseudo_element() {
  let html = r#"
    <div id="host">
      <template shadowrootmode="open">
        <input id="range" type="range" exportparts="::slider-thumb: thumb">
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let map = compute_part_export_map_with_ids(&dom, &ids).expect("part export map");

  let host_id = node_id_by_html_id(&dom, &ids, "host");
  let range_id = node_id_by_html_id(&dom, &ids, "range");

  let exports = map.exports_for_host(host_id).expect("host exports");
  let targets = exports.get("thumb").expect("slider thumb exports");
  assert!(
    targets.contains(&ExportedPartTarget::Pseudo {
      node_id: range_id,
      pseudo: PseudoElement::SliderThumb,
    }),
    "exportparts must expose the element's ::slider-thumb pseudo-element as a part target"
  );
}
