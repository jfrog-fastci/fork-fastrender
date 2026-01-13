// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");
  const map1 = el.attributes;
  const map2 = el.attributes;

  assert_true(map1 instanceof NamedNodeMap, "attributes should be a NamedNodeMap");
  assert_equals(map1, map2, "Element.attributes must be [SameObject]");
}, "Element.attributes returns a [SameObject] NamedNodeMap");

test(() => {
  const el = document.createElement("div");
  const attrs = el.attributes;

  assert_equals(attrs.length, 0, "empty element should have 0 attributes");

  el.setAttribute("id", "a");
  assert_equals(attrs.length, 1, "length should update after setAttribute");

  el.setAttribute("class", "b");
  assert_equals(attrs.length, 2, "length should update after adding a second attribute");

  el.removeAttribute("id");
  assert_equals(attrs.length, 1, "length should update after removeAttribute");
}, "NamedNodeMap.length is live");

test(() => {
  const el = document.createElement("div");
  el.setAttribute("id", "a");
  el.setAttribute("class", "b");

  const attrs = el.attributes;

  assert_true(attrs[0] instanceof Attr, "indexed access should return an Attr");
  assert_true(attrs[0] instanceof Node, "Attr objects should inherit from Node");
  assert_equals(attrs[0].name, "id");
  assert_equals(attrs[0].value, "a");
  assert_equals(attrs[0].ownerElement, el, "Attr.ownerElement should be the owning element");

  assert_equals(attrs.item(1).name, "class", "item(index) should return the attribute in order");
  assert_equals(attrs.item(2), null, "item(out of range) should return null");

  assert_equals(attrs.getNamedItem("class").value, "b", "getNamedItem should find attributes by name");
  assert_equals(attrs.getNamedItem("missing"), null, "getNamedItem should return null when missing");
}, "NamedNodeMap index access and lookup methods return Attr objects");

test(() => {
  const attr = document.createAttribute("id");
  assert_true(attr instanceof Attr, "createAttribute should return an Attr");
  assert_true(attr instanceof Node, "Attr objects should inherit from Node");
  assert_equals(attr.name, "id");
  assert_equals(attr.value, "");
  assert_equals(attr.ownerElement, null);
}, "Document.createAttribute returns a branded Attr object");

