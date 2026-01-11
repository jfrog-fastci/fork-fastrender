// EXPECT: ok
globalThis.__native_result = "pending";

// `Promise.withResolvers()` returns `{ promise, resolve, reject }`.
const r = Promise.withResolvers();
r.promise.then((v: string) => {
  globalThis.__native_result = v;
});
r.resolve("ok");
