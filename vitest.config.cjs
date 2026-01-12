// Root Vitest configuration.
//
// We invoke Vitest from the repository root in CI (see `vendor/ecma-rs/instructions/native_aot.md`),
// but only `optimize-js-debugger` contains Vitest tests. Other vendored fixtures (e.g. the
// TypeScript ESLint rule tests under `vendor/ecma-rs/parse-js/tests/TypeScript`) are *not* meant to
// be executed by Vitest and may require additional tooling (like Mocha).
//
// Restrict the test glob so `vitest run` is deterministic and doesn't accidentally execute those
// vendored suites.
module.exports = {
  test: {
    include: ["vendor/ecma-rs/optimize-js-debugger/src/**/*.test.{ts,tsx}"],
  },
};

