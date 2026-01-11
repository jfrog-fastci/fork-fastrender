// EXPECT: true,true,false
const xs = [2, 4, 6];
const allEven = xs.every((x) => x % 2 === 0);
const someGt5 = xs.some((x) => x > 5);
const someOdd = xs.some((x) => x % 2 !== 0);
globalThis.__native_result = `${allEven},${someGt5},${someOdd}`;

