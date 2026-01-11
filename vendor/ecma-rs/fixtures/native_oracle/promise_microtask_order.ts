// EXPECT: bac
globalThis.__native_result = "";
Promise.resolve().then(() => {
  globalThis.__native_result += "a";
});
globalThis.__native_result += "b";
Promise.resolve().then(() => {
  globalThis.__native_result += "c";
});
