// EXPECT: 2,4,6
const xs: number[] = [1, 2, 3];
globalThis.__native_result = xs.map((x) => x * 2).join(",");

