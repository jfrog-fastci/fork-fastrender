// EXPECT: ba
globalThis.__native_result = "";

// Resolving a promise should not run `.then` handlers synchronously.
const r = Promise.withResolvers();
r.promise.then(() => {
  globalThis.__native_result += "a";
});
globalThis.__native_result += "b";
r.resolve("ok");
