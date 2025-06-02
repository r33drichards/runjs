
console.log({
    "foo": "bar"
})
console.log(
    await runjs.readFile('example.ts')
)

console.log(await runjs.readFile('/Users/robertwendt/.zshrc'))