// META: script=/resources/testharness.js
//
// Minimal instanceof coverage for common Event subclasses.

test(function () {
  var ev = new UIEvent("x");
  assert_true(ev instanceof UIEvent, "UIEvent instanceof UIEvent");
  assert_true(ev instanceof Event, "UIEvent instanceof Event");
}, "UIEvent instanceof Event");

test(function () {
  var ev = new MouseEvent("click");
  assert_true(ev instanceof MouseEvent, "MouseEvent instanceof MouseEvent");
  assert_true(ev instanceof UIEvent, "MouseEvent instanceof UIEvent");
  assert_true(ev instanceof Event, "MouseEvent instanceof Event");
}, "MouseEvent instanceof UIEvent and Event");

test(function () {
  var ev = new KeyboardEvent("keydown");
  assert_true(ev instanceof KeyboardEvent, "KeyboardEvent instanceof KeyboardEvent");
  assert_true(ev instanceof UIEvent, "KeyboardEvent instanceof UIEvent");
  assert_true(ev instanceof Event, "KeyboardEvent instanceof Event");
}, "KeyboardEvent instanceof UIEvent and Event");

test(function () {
  var ev = new FocusEvent("focus");
  assert_true(ev instanceof FocusEvent, "FocusEvent instanceof FocusEvent");
  assert_true(ev instanceof UIEvent, "FocusEvent instanceof UIEvent");
  assert_true(ev instanceof Event, "FocusEvent instanceof Event");
}, "FocusEvent instanceof UIEvent and Event");

test(function () {
  var ev = new InputEvent("input");
  assert_true(ev instanceof InputEvent, "InputEvent instanceof InputEvent");
  assert_true(ev instanceof UIEvent, "InputEvent instanceof UIEvent");
  assert_true(ev instanceof Event, "InputEvent instanceof Event");
}, "InputEvent instanceof UIEvent and Event");

