// META: script=/resources/testharness.js
// META: script=/resources/meta_dep.js

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

if (globalThis.__meta_dep_loaded !== true) {
  report_fail("META dependency should have executed");
} else if (location.href !== "https://web-platform.test/smoke/meta_script.window.js") {
  report_fail("location.href should be the WPT test URL");
} else {
  report_pass();
}
