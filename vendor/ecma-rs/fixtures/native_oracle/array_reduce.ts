// EXPECT: 10
const xs = [1, 2, 3, 4];
globalThis.__native_result = xs.reduce((acc, x) => acc + x, 0);

