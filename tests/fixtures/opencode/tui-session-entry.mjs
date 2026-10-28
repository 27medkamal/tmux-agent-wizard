// OpenCode 2 loads `taw-opencode/tui.js`, which is the version 1 plugin file's own default
// export: the same object, not a copy.

import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";

const v1 = (await import(pathToFileURL(process.env.TAW_PLUGIN).href)).default;
const v2 = (await import(pathToFileURL(process.env.TAW_V2_ENTRY).href)).default;
assert.equal(v2, v1);
assert.equal(v2.id, "taw.opencode.session-selection");
assert.equal(typeof v2.tui, "function");
assert.equal(typeof v2.setup, "function");
