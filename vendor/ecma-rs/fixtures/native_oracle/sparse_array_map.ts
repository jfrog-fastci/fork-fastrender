// EXPECT: 2:2,,6
const arr = [1, , 3];
let calls = 0;
const out = arr.map((v) => {
  calls += 1;
  return v * 2;
});
globalThis.__native_result = `${calls}:${out.join(",")}`;

