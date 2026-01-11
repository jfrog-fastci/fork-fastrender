// EXPECT: 3
const xs = [1, 2, 3, 4];
globalThis.__native_result = xs.find((x) => x > 2);

