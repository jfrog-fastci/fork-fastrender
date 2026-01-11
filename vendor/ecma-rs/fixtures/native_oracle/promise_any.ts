// EXPECT: yes
globalThis.__native_result = "pending";
Promise.any([Promise.reject("no"), Promise.resolve("yes")]).then((v) => {
  globalThis.__native_result = v;
});
