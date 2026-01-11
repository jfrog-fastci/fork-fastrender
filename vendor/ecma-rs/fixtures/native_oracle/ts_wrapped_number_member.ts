// EXPECT: function|function|function
globalThis.__native_result =
  typeof (1 as number).toString +
  "|" +
  typeof (1!).toString +
  "|" +
  typeof (1 satisfies number).toString;
