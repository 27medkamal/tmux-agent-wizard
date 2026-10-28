// `node --import` this file, and every later `import "node:child_process"` gets the fake.
//
// `registerHooks` (node 22.15 and later) runs the hook in this thread; `register`, which older
// nodes have, runs it in a thread of its own and is deprecated where `registerHooks` exists.

import * as module from "node:module";

import { resolve } from "./child-process-hooks.mjs";

if (typeof module.registerHooks === "function") {
  module.registerHooks({ resolve });
} else {
  module.register("./child-process-hooks.mjs", import.meta.url);
}
