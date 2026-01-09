test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const c = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(c);
  const nodes = parent.childNodes;

  const b = document.createElement("span");
  const ret = parent.insertBefore(b, c);
  assert_equals(ret, b);
  assert_equals(parent.childNodes, nodes, "childNodes should be cached");
  assert_equals(nodes.length, 3);
  assert_equals(nodes[0], a);
  assert_equals(nodes[1], b);
  assert_equals(nodes[2], c);
  assert_equals(b.parentNode, parent);
}, "Node.insertBefore inserts before the reference child");

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);
  const nodes = parent.childNodes;

  const ret = parent.insertBefore(a, null);
  assert_equals(ret, a);
  assert_equals(nodes.length, 2);
  assert_equals(nodes[0], b);
  assert_equals(nodes[1], a);
}, "Node.insertBefore with null reference appends and moves existing children");

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const c = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(c);
  const parent_nodes = parent.childNodes;

  const frag = document.createDocumentFragment();
  const x = document.createElement("span");
  const y = document.createElement("span");
  frag.appendChild(x);
  frag.appendChild(y);
  const frag_nodes = frag.childNodes;
  assert_equals(frag_nodes.length, 2);

  const ret = parent.insertBefore(frag, c);
  assert_equals(ret, frag);
  assert_equals(parent.childNodes, parent_nodes, "parent.childNodes should remain cached");
  assert_equals(frag.childNodes, frag_nodes, "fragment.childNodes should remain cached");

  assert_equals(parent_nodes.length, 4);
  assert_equals(parent_nodes[0], a);
  assert_equals(parent_nodes[1], x);
  assert_equals(parent_nodes[2], y);
  assert_equals(parent_nodes[3], c);

  assert_equals(frag_nodes.length, 0, "fragment should be emptied after insertion");
  assert_equals(frag.parentNode, null);
  assert_equals(x.parentNode, parent);
  assert_equals(y.parentNode, parent);
}, "Node.insertBefore supports DocumentFragment insertion semantics");

