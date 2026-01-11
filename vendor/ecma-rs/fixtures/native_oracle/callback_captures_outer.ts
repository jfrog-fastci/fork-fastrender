// EXPECT: 3,6,9
let factor = 3;
const xs = [1, 2, 3];
const ys = xs.map((x) => x * factor);
factor = 4;
globalThis.__native_result = ys.join(",");

