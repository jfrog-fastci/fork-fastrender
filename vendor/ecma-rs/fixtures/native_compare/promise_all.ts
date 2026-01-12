Promise.all([Promise.resolve(1), Promise.resolve(2)]).then((xs) =>
  console.log(xs[0] + xs[1])
);
