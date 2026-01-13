// META: script=/resources/testharness.js
//
// Ensure DOM traversal APIs (NodeIterator/TreeWalker) do not traverse into inert <template>
// contents. FastRender represents template contents as children of the <template> element with
// `inert_subtree=true`; traversal must treat the template as a leaf.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  root.id = "root";

  const tmpl = document.createElement("template");
  tmpl.id = "tmpl";
  tmpl.innerHTML = "<span id=x></span>";

  const after = document.createElement("div");
  after.id = "after";

  root.appendChild(tmpl);
  root.appendChild(after);
  document.body.appendChild(root);

  const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, null);
  const seen = [];
  let node;
  while ((node = it.nextNode())) {
    if (node.id) {
      seen.push(node.id);
    }
    assert_not_equals(
      node.id,
      "x",
      "NodeIterator rooted outside a <template> must not visit nodes inside its inert contents"
    );
  }

  // Tree order should include the root and its real children, but not the template contents.
  assert_array_equals(seen, ["root", "tmpl", "after"]);
}, "NodeIterator does not descend into inert <template> contents");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const tmpl = document.createElement("template");
  tmpl.id = "tmpl";
  tmpl.innerHTML = "<span id=x></span>";
  const after = document.createElement("div");
  after.id = "after";

  root.appendChild(tmpl);
  root.appendChild(after);
  document.body.appendChild(root);

  const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null);
  const seen = [];
  let node;
  while ((node = tw.nextNode())) {
    if (node.id) {
      seen.push(node.id);
    }
    assert_not_equals(
      node.id,
      "x",
      "TreeWalker rooted outside a <template> must not visit nodes inside its inert contents"
    );
  }

  // TreeWalker.nextNode() does not return the root; it returns descendants in tree order.
  assert_array_equals(seen, ["tmpl", "after"]);
}, "TreeWalker does not descend into inert <template> contents");

test(() => {
  clear_children(document.body);

  const template = document.createElement("template");
  const inner = document.createElement("span");
  template.appendChild(inner);
  document.body.appendChild(template);

  const it = document.createNodeIterator(template, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(it.nextNode(), template);
  assert_equals(it.nextNode(), null);

  const tw = document.createTreeWalker(template, NodeFilter.SHOW_ELEMENT, null);
  assert_equals(tw.nextNode(), null);
}, "Traversal treats <template> itself as a leaf");
