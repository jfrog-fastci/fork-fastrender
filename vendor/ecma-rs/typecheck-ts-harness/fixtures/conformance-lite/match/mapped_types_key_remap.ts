// @lib: es5

type Api<T> = {
  [K in keyof T as `get${Capitalize<string & K>}`]: () => T[K];
};

type Input = { foo: number; bar: string };
type Out = Api<Input>;

declare const api: Out;

export const n: number = api.getFoo();
export const s: string = api.getBar();
