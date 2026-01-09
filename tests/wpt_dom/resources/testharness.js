// Minimal `testharness.js` shim for FastRender's offline DOM WPT runner.
//
// This file is intentionally tiny: the current `vm-js`-backed runner executes only a small
// JavaScript subset, and the curated smoke tests use the `__fastrender_wpt_report(...)` hook
// directly.
//
// As the JS engine grows, we can replace this with a closer upstream `testharness.js` copy.

var PASS = 0;
var FAIL = 1;
var TIMEOUT = 2;
var NOTRUN = 3;

function assert_true(value) {
  if (value !== true) {
    throw "assert_true";
  }
}

function assert_equals(actual, expected) {
  if (actual !== expected) {
    throw "assert_equals";
  }
}

