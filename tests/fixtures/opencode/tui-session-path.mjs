// The path taw was rendered with, however odd, is what the plugin runs: the escaping an install
// applies for a JavaScript string survives being read back.

import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";

import { requests } from "./fake-child-process.mjs";

process.env.TMUX_PANE = "%1";
const plugin = (await import(pathToFileURL(process.env.TAW_PLUGIN).href)).default;
const api = {
  route: { current: { name: "session", params: { sessionID: "session-a" } } },
  client: {
    session: { get: async () => ({ data: { id: "session-a" } }), status: async () => ({ data: {} }) },
    permission: { list: async () => ({ data: [] }) },
    question: { list: async () => ({ data: [] }) },
  },
  event: { on: () => () => {} },
  state: { session: { get: () => ({ id: "session-a" }) } },
  lifecycle: { onDispose() {} },
};
let dispose;
api.lifecycle.onDispose = (handler) => { dispose = handler; };
await plugin.tui(api);
await new Promise((resolve) => setTimeout(resolve, 50));
dispose();
assert.ok(requests.length > 0, "the plugin ran taw");
