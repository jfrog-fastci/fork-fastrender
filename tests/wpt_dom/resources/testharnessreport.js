/*
 * Minimal `testharnessreport.js` shim for FastRender's offline WPT DOM runner.
 *
 * The real WPT `testharnessreport.js` builds a rich HTML report. FastRender's runner only needs
 * `fastrender_testharness_report.js` (injected by the harness) to emit a machine-readable payload.
 *
 * Keep this file intentionally cheap: some DOM tests define hundreds of subtests with long names,
 * and a full HTML report can dominate runtime on the vm-js backend.
 */

(function (global) {
  "use strict";

  if (typeof add_completion_callback !== "function") return;

  function __testharnessreport_noop(_tests, _harnessStatus) {
    // no-op
  }

  add_completion_callback(__testharnessreport_noop);
})(typeof globalThis !== "undefined" ? globalThis : this);

