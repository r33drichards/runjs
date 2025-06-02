
// interface Foo  {

// }
// const f: Foo = {}
console.log("hello");

const file = runjs.readFile('example.ts')
console.log(file)
const path = 'example.ts';
runjs.writeFile(path, 'foo')
const file2 = runjs.readFile('example.ts')
console.log(file2)
