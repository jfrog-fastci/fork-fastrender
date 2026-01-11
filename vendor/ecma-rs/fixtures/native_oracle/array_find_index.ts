// EXPECT: 1
const xs = [1, 2, 3];
globalThis.__native_result = xs.findIndex((x) => x === 2);
