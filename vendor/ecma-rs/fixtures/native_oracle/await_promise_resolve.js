// Returns a Promise<string> that settles during a microtask checkpoint.
//
// This is equivalent to:
//   (async () => await Promise.resolve("ok"))()
// but avoids `async`/`await` syntax (not implemented by `vm-js`'s interpreter yet).
Promise.resolve("ok").then(function (v) {
  return v;
})
