// META: script=/resources/testharness.js
//
// Curated passive listener check.

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

var target = new EventTarget();
var ran = false;

function listener(e) {
  ran = true;
  e.preventDefault();
}

target.addEventListener("x", listener, { passive: true });

var ev = new Event("x", { cancelable: true });
var res = target.dispatchEvent(ev);

if (ran !== true) {
  report_fail("passive listener did not run");
} else if (ev.defaultPrevented !== false) {
  report_fail("preventDefault must be ignored in passive listeners");
} else if (res !== true) {
  report_fail("dispatchEvent must return true when default was not prevented");
} else {
  report_pass();
}
