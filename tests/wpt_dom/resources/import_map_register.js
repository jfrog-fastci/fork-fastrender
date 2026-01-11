// Helper for curated WPT module tests: register an import map through the runner-provided hook.
//
// The offline `js-wpt-dom-runner` installs `globalThis.__fastrender_register_import_map` during
// realm bootstrap. This shim is intentionally tiny so tests can include it as a META script and
// remain close to upstream WPT patterns.
globalThis.__fastrender_register_import_map(
  '{"imports":{"foo":"/resources/mod_mapped.js"}}'
);
