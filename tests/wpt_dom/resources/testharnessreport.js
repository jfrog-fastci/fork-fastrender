/*
 * Minimal `testharnessreport.js` subset for FastRender's offline WPT DOM smoke tests.
 *
 * The real WPT reporter produces a rich HTML report. This file only prints a compact
 * log and exposes results in the DOM for debugging.
 */

(function (global) {
  "use strict";

  if (typeof add_completion_callback !== "function") return;
  if (typeof document === "undefined") return;

  function statusText(status) {
    switch (status) {
      case PASS:
        return "PASS";
      case FAIL:
        return "FAIL";
      case TIMEOUT:
        return "TIMEOUT";
      case NOTRUN:
        return "NOTRUN";
      default:
        return String(status);
    }
  }

  function ensureLogElement() {
    let el = document.getElementById("log");
    if (el) return el;

    el = document.createElement("pre");
    el.id = "log";

    if (document.body) {
      document.body.appendChild(el);
    } else {
      document.documentElement.appendChild(el);
    }

    return el;
  }

  add_completion_callback((tests, harnessStatus) => {
    const el = ensureLogElement();
    const lines = [];

    for (const t of tests) {
      let line = `${statusText(t.status)}: ${t.name}`;
      if (t.message) {
        line += `\n  ${t.message}`;
      }
      lines.push(line);
    }

    if (harnessStatus && harnessStatus.status !== 0) {
      lines.push(`HARNESS ERROR: ${harnessStatus.message || "unknown"}`);
    }

    el.textContent = lines.join("\n");
  });
})(typeof globalThis !== "undefined" ? globalThis : this);

