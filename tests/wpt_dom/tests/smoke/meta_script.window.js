// META: script=/resources/testharness.js
// META: script=/resources/meta_dep.js

test(() => {
  assert_true(globalThis.__meta_dep_loaded, "META dependency should have executed");
  assert_equals(
    location.href,
    "https://web-platform.test/smoke/meta_script.window.js",
    "location.href should be the WPT test URL"
  );
}, "META script loading + location.href smoke test");

