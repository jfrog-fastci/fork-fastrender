// EXPECT: 2,3
const xs: number[] = [1, 2];
globalThis.__native_result = xs.map((x) => x + 1).join(",");
