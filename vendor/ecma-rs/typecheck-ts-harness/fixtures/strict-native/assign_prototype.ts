// @lib: es2015
declare const Foo: { prototype: object };

const Object = {
  assign: (_target: object, _source: object) => {},
};

Object.assign(Foo.prototype, {});
