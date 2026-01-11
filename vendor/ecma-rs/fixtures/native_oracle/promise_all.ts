// EXPECT: ab
globalThis.__native_result = "pending";
Promise.all([Promise.resolve("a"), Promise.resolve("b")]).then((xs) => {
  globalThis.__native_result = xs.join("");
});
