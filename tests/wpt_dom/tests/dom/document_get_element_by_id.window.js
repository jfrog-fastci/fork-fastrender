// META: script=/resources/testharness.js
//
// Curated `Document.getElementById` semantics checks.
//
// This uses the host DOM shims and reports directly via `__fastrender_wpt_report` so it can run on
// the minimal vm-js WPT backend without relying on the full `testharness.js` assertion helpers.

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

var failed = false;

function fail(message) {
  if (failed) return;
  failed = true;
  report_fail(message);
}

function assert_equals(actual, expected, message) {
  if (failed) return;
  if (actual !== expected) fail(message);
}

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function run() {
  clear_children(document.body);

  assert_equals(
    document.getElementById("missing"),
    null,
    "getElementById should return null when the element does not exist"
  );

  var a = document.createElement("div");
  a.id = "a";
  document.body.appendChild(a);

  var b = document.createElement("div");
  b.id = "b";
  document.body.appendChild(b);

  assert_equals(document.getElementById("a"), a, "expected getElementById('a') to return the element");
  assert_equals(document.getElementById("b"), b, "expected getElementById('b') to return the element");

  if (failed) return;

  // Duplicate ids are invalid HTML but allowed in practice; `getElementById` returns the first
  // matching element in tree order.
  var c = document.createElement("div");
  c.id = "a";
  document.body.appendChild(c);

  assert_equals(
    document.getElementById("a"),
    a,
    "expected getElementById to return the first element in tree order when ids collide"
  );

  if (failed) return;

  // `<template>` contents are inert and must not be searched.
  var tmpl = document.createElement("template");
  tmpl.id = "tmpl";
  document.body.appendChild(tmpl);

  var inside = document.createElement("div");
  inside.id = "inside";
  tmpl.appendChild(inside);

  assert_equals(document.getElementById("tmpl"), tmpl, "template element itself should be findable");
  assert_equals(
    document.getElementById("inside"),
    null,
    "getElementById must not search inside inert <template> contents"
  );

  if (failed) return;

  // Detached DocumentFragments should be searchable via `fragment.getElementById`, but their
  // contents should not be visible to `document.getElementById` until inserted.
  var frag = document.createDocumentFragment();
  var f1 = document.createElement("div");
  f1.id = "f1";
  frag.appendChild(f1);

  var f2 = document.createElement("div");
  f2.id = "f2";
  frag.appendChild(f2);

  assert_equals(frag.getElementById("missing"), null, "fragment.getElementById should return null for missing ids");
  assert_equals(frag.getElementById("f1"), f1, "fragment.getElementById should find descendants");
  assert_equals(frag.getElementById("f2"), f2, "fragment.getElementById should find descendants");
  assert_equals(
    document.getElementById("f1"),
    null,
    "document.getElementById must not search detached fragment contents"
  );
}

run();

if (!failed) {
  report_pass();
}
