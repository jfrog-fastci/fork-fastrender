test(() => {
  const container = document.createElement("div");
  const child = document.createElement("span");
  container.appendChild(child);

  assert_true(container.contains(container), "node contains itself");
  assert_true(container.contains(child), "node contains descendant");
  assert_false(child.contains(container), "node does not contain ancestor");
  assert_false(container.contains(null), "null should return false");
  assert_false(container.contains(), "undefined should return false");

  let threw = false;
  try {
    container.contains(1);
  } catch (e) {
    threw = true;
  }
  assert_true(threw, "non-Node argument should throw");
}, "Node.contains basic semantics");

test(() => {
  const root = document.createElement("div");
  const child = document.createElement("span");
  root.appendChild(child);
  document.body.appendChild(root);

  assert_true(document.contains(child), "document contains connected descendant");

  const detached = document.createElement("p");
  assert_false(document.contains(detached), "document does not contain detached nodes");
}, "Document.contains reflects tree connectivity");

test(() => {
  const frag = document.createDocumentFragment();
  const node = document.createElement("span");
  frag.appendChild(node);
  assert_true(frag.contains(node), "DocumentFragment contains its children");

  document.body.appendChild(frag);
  assert_false(frag.contains(node), "DocumentFragment loses children on insertion");
  assert_true(document.body.contains(node), "moved child is now contained by the parent");
}, "Node.contains with DocumentFragment insertion semantics");

