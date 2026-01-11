// EXPECT: true,false,true
const xs = [1, 2, NaN];
globalThis.__native_result =
  String(xs.includes(2)) + "," + String(xs.includes(3)) + "," + String(xs.includes(NaN));
