// Runs the installed TUI plugin against a real taw and a real tmux pane, one step at a time:
// after each step it says `done <step>` and waits for a line on standard input, so that the
// test can read the pane's options while the plugin is idle. The TUI is a small stand-in for
// what OpenCode 1 hands a plugin (`tui(api)`); the plugin itself, the `execFile` of taw and
// every option in tmux are the real ones.

import readline from "node:readline";
import { pathToFileURL } from "node:url";

const plugin = (await import(pathToFileURL(process.env.TAW_PLUGIN).href)).default;

// The ids are OpenCode's own shape (`ses_...`), handed in by the test that reads them back.
const ROOT = process.env.TAW_SESSION;
const CHILD = process.env.TAW_CHILD_SESSION;
const sessions = new Map([
  [ROOT, { id: ROOT }],
  [CHILD, { id: CHILD, parentID: ROOT }],
]);
const listeners = new Map();
let route = { name: "home" };
let dispose;
const api = {
  client: {
    session: {
      get: async ({ sessionID }) => ({ data: sessions.get(sessionID) }),
      status: async () => ({ data: {} }),
    },
    permission: { list: async () => ({ data: [] }) },
    question: { list: async () => ({ data: [] }) },
  },
  event: {
    on(type, receive) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(receive);
      return () => listeners.get(type).delete(receive);
    },
  },
  route: {
    get current() {
      return route;
    },
  },
  state: { session: { get: (id) => sessions.get(id) } },
  lifecycle: {
    onDispose(handler) {
      dispose = handler;
      return () => {};
    },
  },
};
const emit = (type, properties) => {
  for (const receive of listeners.get(type) ?? []) receive({ type, properties });
};
const select = (sessionID) => {
  route = { name: "session", params: { sessionID } };
};

await plugin.tui(api);

const lines = readline.createInterface({ input: process.stdin });
const next = () => new Promise((resolve) => lines.once("line", resolve));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function step(name, action) {
  action();
  // The reports queue behind one another, and each is a run of taw.
  await sleep(600);
  console.log(`done ${name}`);
  await next();
}

await step("select", () => select(ROOT));
await step("working", () => emit("session.status", { sessionID: ROOT, status: { type: "busy" } }));
await step("child-blocked", () => emit("permission.asked", { id: "p1", sessionID: CHILD }));
await step("child-replied", () => emit("permission.replied", { requestID: "p1", sessionID: CHILD }));
await step("idle", () => emit("session.idle", { sessionID: ROOT }));
await step("home", () => {
  route = { name: "home" };
  emit("session.updated", { info: { id: ROOT } });
});
dispose();
lines.close();
