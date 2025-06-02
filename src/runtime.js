const { core } = Deno;

function argsToMessage(...args) {
  return args.map((arg) => JSON.stringify(arg)).join(" ");
}

globalThis.console = {
  log: (...args) => {
    core.print(`${argsToMessage(...args)}\n`, false);
  },
  error: (...args) => {
    core.print(`${argsToMessage(...args)}\n`, true);
  },
};

globalThis.runjs = {
  readFile: async (path) => {
    return await core.ops.op_read_file(path);
  },
  writeFile: async (path, contents) => {
    return await core.ops.op_write_file(path, contents);
  },
  removeFile: (path) => {
    return core.ops.op_remove_file(path);
  },
  fetch: async (url) => {
    return await core.ops.op_fetch(url);
  },
};

globalThis.setTimeout = async (delay) => {
  await core.ops.op_set_timeout(delay);
};