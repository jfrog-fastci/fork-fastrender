// EXPECT: fulfilled:a,rejected:no
globalThis.__native_result = "pending";
Promise.allSettled([Promise.resolve("a"), Promise.reject("no")]).then((results) => {
  globalThis.__native_result =
    results[0].status +
    ":" +
    results[0].value +
    "," +
    results[1].status +
    ":" +
    results[1].reason;
});
