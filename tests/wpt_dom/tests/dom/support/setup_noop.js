// Minimal `setup()` shim used by tests that import `dom/common.js`.
//
// `dom/common.js` eagerly calls `setup(setupRangeTests)` when a `setup` global is present; when it
// isn't, it falls back to running `setupRangeTests()` immediately, which requires a full DOM.
//
// Some focused unit tests (like `getDomExceptionName.window.js`) only need helpers from `common.js`
// and should not require the Range fixture DOM to exist at load time. Installing this stub before
// importing `common.js` suppresses that eager fixture creation.

if (!("setup" in window)) {
  window.setup = function (_fn) {};
}

