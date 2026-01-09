// META: script=/resources/testharness.js
// META: timeout=long

function done() {
  __fastrender_wpt_report({ file_status: "pass" });
}

// Intentionally non-zero to ensure the runner honors META timeout=long when its default timeout
// is configured to be very short (validated in Rust integration tests).
setTimeout(done, 50);
