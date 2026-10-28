// The module hook for the plugin tests: `node:child_process` is answered by the fake next to
// this file, so a plugin that runs taw runs nothing, and the test reads what it asked for.

export function resolve(specifier, context, nextResolve) {
  if (specifier === "node:child_process") {
    return {
      url: new URL("./fake-child-process.mjs", import.meta.url).href,
      shortCircuit: true,
    };
  }
  return nextResolve(specifier, context);
}
