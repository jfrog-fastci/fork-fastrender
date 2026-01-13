// META: script=/resources/testharness.js
//
// Modern DOM convenience mutation APIs:
// - ParentNode.replaceChildren
// - ChildNode.before/after/replaceWith/remove (co-located so it runs under the replaceChildren filter)

function clear_children(node) {
  while (node.firstChild) {
    node.removeChild(node.firstChild);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const old_a = document.createElement("span");
  old_a.id = "a";
  const old_b = document.createElement("span");
  old_b.id = "b";
  parent.appendChild(old_a);
  parent.appendChild(old_b);

  // Cache live collections before mutation.
  const cached_child_nodes = parent.childNodes;
  const cached_children = parent.children;

  const new_el = document.createElement("span");
  new_el.id = "new";

  const ret = parent.replaceChildren("x", new_el);
  assert_equals(ret, undefined, "replaceChildren() should return undefined");

  assert_equals(parent.childNodes, cached_child_nodes, "childNodes should remain cached");
  assert_equals(parent.children, cached_children, "children should remain cached");

  assert_equals(cached_child_nodes.length, 2, "expected two childNodes after replacement");
  assert_equals(cached_child_nodes[0].nodeType, Node.TEXT_NODE, "expected first child to be Text");
  assert_equals(cached_child_nodes[0].data, "x", "expected inserted Text node content");
  assert_equals(cached_child_nodes[1], new_el, "expected inserted element node");

  assert_equals(cached_children.length, 1, "expected one element child after replacement");
  assert_equals(cached_children[0], new_el, "expected new_el to be the only element child");

  assert_equals(old_a.parentNode, null, "expected old_a to be detached");
  assert_equals(old_b.parentNode, null, "expected old_b to be detached");
}, "ParentNode.replaceChildren replaces all children and keeps live collections up to date");

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);
  parent.appendChild(document.createElement("span"));
  parent.appendChild(document.createTextNode("y"));

  parent.replaceChildren();
  assert_equals(parent.childNodes.length, 0, "expected replaceChildren() with no args to clear children");
  assert_equals(parent.children.length, 0, "expected replaceChildren() with no args to clear element children");
}, "ParentNode.replaceChildren() with no arguments removes all children");

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const a = document.createElement("span");
  a.id = "a";
  const b = document.createElement("span");
  b.id = "b";
  const c = document.createElement("span");
  c.id = "c";
  parent.appendChild(a);
  parent.appendChild(b);
  parent.appendChild(c);

  b.before("x");
  assert_equals(parent.childNodes[0], a);
  assert_equals(parent.childNodes[1].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[1].data, "x");
  assert_equals(parent.childNodes[2], b);

  b.after("y");
  assert_equals(parent.childNodes[3].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[3].data, "y");
  assert_equals(parent.childNodes[4], c);

  b.replaceWith("z");
  assert_equals(parent.childNodes[2].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[2].data, "z");

  // `replaceWith()` with no arguments removes the node.
  c.replaceWith();
  assert_equals(parent.querySelector("#c"), null, "expected c to be removed by replaceWith()");

  a.remove();
  assert_equals(parent.querySelector("#a"), null, "expected a to be removed by remove()");
}, "ChildNode before/after/replaceWith/remove work and convert strings to Text");

