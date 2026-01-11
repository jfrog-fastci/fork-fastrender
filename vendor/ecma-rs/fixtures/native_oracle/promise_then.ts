// EXPECT: then:2
globalThis.__native_result = "init";
new Promise((resolve) => resolve(1)).then((v) => {
  globalThis.__native_result = `then:${v + 1}`;
});
globalThis.__native_result += "|sync";

