// EXPECT: b
globalThis.__native_result = "pending";

// `Promise.race` should resolve with the value of the first promise to settle.
let resolveA = (_v: string) => {};
let resolveB = (_v: string) => {};
const p1 = new Promise<string>((resolve) => {
  resolveA = resolve;
});
const p2 = new Promise<string>((resolve) => {
  resolveB = resolve;
});

Promise.race([p1, p2]).then((v) => {
  globalThis.__native_result = v;
});

// Resolve out-of-order: p2 first.
resolveB("b");
resolveA("a");
