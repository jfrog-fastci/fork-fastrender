// EXPECT: 3,6,9
const ctx = { mult: 3 };
const xs = [1, 2, 3];
const out = xs.map(function (x) {
  return x * this.mult;
}, ctx);
globalThis.__native_result = out.join(",");

