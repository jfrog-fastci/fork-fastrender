// META: script=/resources/testharness.js
// META: script=/resources/meta_dep.js

var ran = false;

function report(payload) {
  __fastrender_wpt_report(payload);
}

function on_fulfilled(v) {
  ran = true;

  if (v !== 42) {
    report({ file_status: "fail", message: "Promise should resolve to 42" });
    return;
  }

  if (globalThis.__meta_dep_loaded !== true) {
    report({ file_status: "fail", message: "META dependency should have executed" });
    return;
  }

  if (location.href !== "https://web-platform.test/smoke/any_promise.any.js") {
    report({ file_status: "fail", message: "location.href should be the WPT test URL" });
    return;
  }

  report({ file_status: "pass" });
}

Promise.resolve(42).then(on_fulfilled);

// Then callbacks should run in a microtask, not synchronously.
if (ran) {
  report({ file_status: "fail", message: "Promise.then ran synchronously" });
}
