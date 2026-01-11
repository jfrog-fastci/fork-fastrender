// EXPECT: 1
var fn = (x) => x;
globalThis.__native_result = fn?.<string>(1);
