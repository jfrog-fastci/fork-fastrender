// @lib: es5

type Box<T = string> = { value: T };

export const boxed: Box = { value: "ok" };
export const value: string = boxed.value;
