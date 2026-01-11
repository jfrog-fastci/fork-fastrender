// EXPECT: a
globalThis.__native_result = "pending";
Promise.race([Promise.resolve("a"), Promise.resolve("b")]).then((v) => {
  globalThis.__native_result = v;
});
