try {
  if (typeof import.meta.resolve !== "function") {
    throw new TypeError("import.meta.resolve is not implemented");
  }
  globalThis.resolved_foo = import.meta.resolve("foo");
  globalThis.resolve_threw = false;
} catch (e) {
  globalThis.resolved_foo = "";
  globalThis.resolve_threw = true;
}

