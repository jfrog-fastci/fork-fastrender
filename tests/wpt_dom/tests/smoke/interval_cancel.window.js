// META: script=/resources/testharness.js

var fired = false;
var interval_id = 0;

function report(payload) {
  __fastrender_wpt_report(payload);
}

function finish() {
  if (fired !== true) {
    report({ file_status: "fail", message: "interval did not fire" });
    return;
  }
  report({ file_status: "pass" });
}

function tick() {
  if (fired) {
    report({ file_status: "fail", message: "interval fired more than once" });
    return;
  }
  fired = true;
  clearInterval(interval_id);
  setTimeout(finish, 10);
}

interval_id = setInterval(tick, 0);

