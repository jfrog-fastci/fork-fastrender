// EXPECT: true,false
const s = "hello";
globalThis.__native_result =
  String(s.includes("ell")) + "," + String(s.includes("xyz"));
