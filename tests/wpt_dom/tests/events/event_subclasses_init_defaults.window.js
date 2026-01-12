// META: script=/resources/testharness.js
//
// Minimal attribute coverage for common Event subclasses. The vm-js backend only implements a
// small, spec-shaped subset of each *EventInit dictionary.

test(function () {
  var ev = new UIEvent("x");
  assert_equals(ev.detail, 0, "UIEvent.detail default");
  assert_equals(ev.view, null, "UIEvent.view default");

  var ev2 = new UIEvent("x", { detail: 7, view: window });
  assert_equals(ev2.detail, 7, "UIEvent.detail init override");
  assert_equals(ev2.view, window, "UIEvent.view init override");
}, "UIEvent init defaults and overrides");

test(function () {
  var ev = new MouseEvent("x");
  assert_equals(ev.clientX, 0, "MouseEvent.clientX default");
  assert_equals(ev.clientY, 0, "MouseEvent.clientY default");
  assert_equals(ev.screenX, 0, "MouseEvent.screenX default");
  assert_equals(ev.screenY, 0, "MouseEvent.screenY default");
  assert_equals(ev.button, 0, "MouseEvent.button default");
  assert_equals(ev.buttons, 0, "MouseEvent.buttons default");
  assert_equals(ev.ctrlKey, false, "MouseEvent.ctrlKey default");
  assert_equals(ev.shiftKey, false, "MouseEvent.shiftKey default");
  assert_equals(ev.altKey, false, "MouseEvent.altKey default");
  assert_equals(ev.metaKey, false, "MouseEvent.metaKey default");
  assert_equals(ev.relatedTarget, null, "MouseEvent.relatedTarget default");

  var rel = document.body;
  var ev2 = new MouseEvent("x", {
    clientX: 10,
    clientY: 11,
    ctrlKey: true,
    relatedTarget: rel,
  });
  assert_equals(ev2.clientX, 10, "MouseEvent.clientX init override");
  assert_equals(ev2.clientY, 11, "MouseEvent.clientY init override");
  assert_equals(ev2.ctrlKey, true, "MouseEvent.ctrlKey init override");
  assert_equals(ev2.relatedTarget, rel, "MouseEvent.relatedTarget init override");
}, "MouseEvent init defaults and overrides");

test(function () {
  var ev = new KeyboardEvent("keydown");
  assert_equals(ev.key, "", "KeyboardEvent.key default");
  assert_equals(ev.code, "", "KeyboardEvent.code default");
  assert_equals(ev.location, 0, "KeyboardEvent.location default");
  assert_equals(ev.ctrlKey, false, "KeyboardEvent.ctrlKey default");
  assert_equals(ev.shiftKey, false, "KeyboardEvent.shiftKey default");
  assert_equals(ev.altKey, false, "KeyboardEvent.altKey default");
  assert_equals(ev.metaKey, false, "KeyboardEvent.metaKey default");
  assert_equals(ev.repeat, false, "KeyboardEvent.repeat default");

  var ev2 = new KeyboardEvent("keydown", {
    key: "a",
    code: "KeyA",
    location: 1,
    ctrlKey: true,
    repeat: true,
  });
  assert_equals(ev2.key, "a", "KeyboardEvent.key init override");
  assert_equals(ev2.code, "KeyA", "KeyboardEvent.code init override");
  assert_equals(ev2.location, 1, "KeyboardEvent.location init override");
  assert_equals(ev2.ctrlKey, true, "KeyboardEvent.ctrlKey init override");
  assert_equals(ev2.repeat, true, "KeyboardEvent.repeat init override");
}, "KeyboardEvent init defaults and overrides");

test(function () {
  var ev = new FocusEvent("focus");
  assert_equals(ev.relatedTarget, null, "FocusEvent.relatedTarget default");

  var rel = document.body;
  var ev2 = new FocusEvent("focus", { relatedTarget: rel });
  assert_equals(ev2.relatedTarget, rel, "FocusEvent.relatedTarget init override");
}, "FocusEvent init defaults and overrides");

test(function () {
  var ev = new InputEvent("input");
  assert_equals(ev.data, null, "InputEvent.data default");
  assert_equals(ev.inputType, "", "InputEvent.inputType default");
  assert_equals(ev.isComposing, false, "InputEvent.isComposing default");

  var ev2 = new InputEvent("input", {
    data: "x",
    inputType: "insertText",
    isComposing: true,
  });
  assert_equals(ev2.data, "x", "InputEvent.data init override");
  assert_equals(ev2.inputType, "insertText", "InputEvent.inputType init override");
  assert_equals(ev2.isComposing, true, "InputEvent.isComposing init override");
}, "InputEvent init defaults and overrides");

