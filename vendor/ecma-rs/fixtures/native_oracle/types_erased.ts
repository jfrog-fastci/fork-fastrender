// EXPECT: 3
type Num = number;
interface Box {
  value: Num;
}

function add(a: Num, b: Num): Num {
  return a + b;
}

globalThis.__native_result = add(1, 2);
