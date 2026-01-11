// EXPECT: 9
const data = ["a", "bb", "ccc", "dddd"];
globalThis.__native_result = data
  .map((s) => s.length)
  .filter((n) => n > 1)
  .reduce((acc, n) => acc + n, 0);

