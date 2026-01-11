// Returns a Promise<string> that settles during a microtask checkpoint.
//
// This is equivalent to:
//   (async () => { const v = await Promise.resolve("ok"); return v; })()
// and ensures the oracle VM supports `async`/`await` syntax (including `await` in a variable
// declarator initializer).
(async () => {
  const v = await Promise.resolve("ok");
  return v;
})()
