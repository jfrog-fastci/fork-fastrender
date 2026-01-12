// META: script=/resources/testharness.js

function assert_array_equals_local(actual, expected, message) {
  assert_equals(actual.length, expected.length, message);
  for (let i = 0; i < expected.length; i++) {
    assert_equals(actual[i], expected[i], `${message} (index ${i})`);
  }
}

test(() => {
  const el = document.createElement("div");
  el.setAttribute("DATA-TEST", "1");
  assert_true(el.hasAttribute("data-test"), "HTML attribute matching must be ASCII case-insensitive");
  assert_true(el.hasAttribute("DaTa-TeSt"), "HTML attribute matching must be ASCII case-insensitive");
  assert_false(el.hasAttribute("missing"));
}, "Element.hasAttribute matches HTML attributes case-insensitively");

test(() => {
  const el = document.createElement("div");
  assert_false(el.hasAttribute("hidden"));
  assert_true(el.toggleAttribute("hidden"), "toggleAttribute() should add when absent");
  assert_true(el.hasAttribute("hidden"));
  assert_false(el.toggleAttribute("hidden"), "toggleAttribute() should remove when present");
  assert_false(el.hasAttribute("hidden"));
}, "Element.toggleAttribute toggles when force is omitted");

test(() => {
  const el = document.createElement("div");
  assert_true(el.toggleAttribute("hidden", true));
  assert_true(el.hasAttribute("hidden"));
  assert_equals(el.getAttribute("hidden"), "", "toggleAttribute(force=true) should create an empty-string attribute");

  assert_false(el.toggleAttribute("hidden", false));
  assert_false(el.hasAttribute("hidden"));
}, "Element.toggleAttribute(force) returns final presence and uses empty string when adding");

test(() => {
  const el = document.createElement("div");
  el.setAttribute("data-test", "x");
  assert_true(el.toggleAttribute("data-test", true));
  assert_equals(
    el.getAttribute("data-test"),
    "x",
    "toggleAttribute(force=true) must not clobber an existing attribute value"
  );
}, "Element.toggleAttribute(force=true) preserves existing attribute value");

test(() => {
  const el = document.createElement("div");
  assert_array_equals_local(el.getAttributeNames(), [], "expected no attributes");

  el.setAttribute("ID", "a");
  el.setAttribute("class", "b");
  assert_array_equals_local(
    el.getAttributeNames(),
    ["id", "class"],
    "getAttributeNames() should return HTML attribute names ASCII-lowercased in insertion order"
  );

  // Updating an existing attribute must not change insertion order.
  el.setAttribute("id", "c");
  assert_array_equals_local(el.getAttributeNames(), ["id", "class"], "expected stable insertion order");
}, "Element.getAttributeNames returns lowercased HTML names in insertion order");

test(() => {
  const el = document.createElement("div");

  el.setAttributeNS(null, "id", "a");
  assert_equals(el.getAttributeNS(null, "id"), "a");

  el.removeAttributeNS(null, "id");
  assert_equals(el.getAttributeNS(null, "id"), null);
}, "Element namespace attribute variants exist and work for null namespace");

test(() => {
  const el = document.createElement("div");
  el.setAttribute("id", "a");

  const observer = new MutationObserver(() => {});
  observer.observe(el, { attributes: true });

  // No-op: force-present when already present.
  el.toggleAttribute("id", true);
  assert_equals(observer.takeRecords().length, 0, "no-op toggleAttribute must not queue mutation records");

  observer.disconnect();
}, "No-op toggleAttribute does not queue MutationObserver records");

test(() => {
  const el = document.createElement("div");
  el.setAttributeNS(null, "id", "a");

  const observer = new MutationObserver(() => {});
  observer.observe(el, { attributes: true });

  // No-op: same value.
  el.setAttributeNS(null, "id", "a");
  assert_equals(observer.takeRecords().length, 0, "no-op setAttributeNS must not queue mutation records");

  observer.disconnect();
}, "No-op setAttributeNS does not queue MutationObserver records");
