async function f() {
  return await Promise.resolve(1);
}
f().then((v) => v + 1);

