// EXPECT: 2,4
const xs = [1, 2, 3, 4];
globalThis.__native_result = xs.filter((x) => x % 2 === 0).join(",");

