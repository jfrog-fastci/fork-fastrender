// @lib: es5

type IsString<T> = T extends string ? true : false;

type A = IsString<string>;

const v: false = (null as any as A);
void v;
