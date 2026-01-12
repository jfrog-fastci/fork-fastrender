// @lib: es5

type IsString<T> = T extends string ? true : false;

type A = IsString<string>;
type B = IsString<number>;

const a: true = (null as any as A);
const b: false = (null as any as B);

void a;
void b;
