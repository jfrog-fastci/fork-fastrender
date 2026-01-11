// EXPECT: 2
function id<T>(x: T): T {
  return x;
}

globalThis.__native_result = id<number>(2);
