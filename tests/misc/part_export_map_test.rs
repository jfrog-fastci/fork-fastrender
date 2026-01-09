use std::collections::HashMap;

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
  assert!(targets.contains(&outer_part_id));
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
  assert!(targets.contains(&inner_part_id));
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
  assert!(targets.contains(&inner_part_id));
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
    exports
      .values()
      .all(|targets| !targets.contains(&inner_part_id)),
    "outer host exports must not include inner shadow part without exportparts"
  );
}
