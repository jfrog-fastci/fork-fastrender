// @lib: es5

type Box<T> = { value: T };

const b: Box<number> = { value: "not a number" };
void b;
