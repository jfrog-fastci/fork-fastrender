test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  const c = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);
  parent.appendChild(c);
  const nodes = parent.childNodes;

  const d = document.createElement("span");
  const ret = parent.replaceChild(d, b);
  assert_equals(ret, b);
  assert_equals(parent.childNodes, nodes, "childNodes should be cached");
  assert_equals(nodes.length, 3);
  assert_equals(nodes[0], a);
  assert_equals(nodes[1], d);
  assert_equals(nodes[2], c);
  assert_equals(b.parentNode, null);
  assert_equals(d.parentNode, parent);
}, "Node.replaceChild replaces an existing child");

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  const c = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);
  parent.appendChild(c);
  const parent_nodes = parent.childNodes;

  const frag = document.createDocumentFragment();
  const x = document.createElement("span");
  const y = document.createElement("span");
  frag.appendChild(x);
  frag.appendChild(y);
  const frag_nodes = frag.childNodes;

  const ret = parent.replaceChild(frag, b);
  assert_equals(ret, b);
  assert_equals(parent.childNodes, parent_nodes, "parent.childNodes should remain cached");
  assert_equals(frag.childNodes, frag_nodes, "fragment.childNodes should remain cached");

  assert_equals(parent_nodes.length, 4);
  assert_equals(parent_nodes[0], a);
  assert_equals(parent_nodes[1], x);
  assert_equals(parent_nodes[2], y);
  assert_equals(parent_nodes[3], c);

  assert_equals(frag_nodes.length, 0, "fragment should be emptied after insertion");
  assert_equals(frag.parentNode, null);
  assert_equals(b.parentNode, null);
  assert_equals(x.parentNode, parent);
  assert_equals(y.parentNode, parent);
}, "Node.replaceChild supports DocumentFragment insertion semantics");

test(() => {
  const parent_a = document.createElement("div");
  const parent_b = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  parent_a.appendChild(a);
  parent_a.appendChild(b);
  const nodes_a = parent_a.childNodes;

  const x = document.createElement("span");
  parent_b.appendChild(x);
  const nodes_b = parent_b.childNodes;

  const ret = parent_a.replaceChild(x, a);
  assert_equals(ret, a);
  assert_equals(nodes_a.length, 2);
  assert_equals(nodes_a[0], x);
  assert_equals(nodes_a[1], b);
  assert_equals(nodes_b.length, 0);
  assert_equals(x.parentNode, parent_a);
  assert_equals(a.parentNode, null);
}, "Node.replaceChild detaches the replacement node from its old parent");

