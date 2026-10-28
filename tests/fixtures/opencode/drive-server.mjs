// Runs the installed server plugin the way `opencode run` does, against a real taw and a real
// tmux pane, one step at a time (see drive-tui.mjs).

import readline from "node:readline";
import { pathToFileURL } from "node:url";

process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
const module = await import(pathToFileURL(process.env.TAW_PLUGIN).href);
const hooks = await module.default.server();

const lines = readline.createInterface({ input: process.stdin });
const next = () => new Promise((resolve) => lines.once("line", resolve));

async function step(name, action) {
  await action();
  console.log(`done ${name}`);
  await next();
}

// OpenCode's own shape of session id (`ses_...`), handed in by the test that reads it back.
const RUN = process.env.TAW_RUN_SESSION;

await step("working", () => hooks["chat.message"]({ sessionID: RUN }));
await step("blocked", () => hooks.event({
  event: { type: "permission.asked", properties: { sessionID: RUN } },
}));
await step("idle", () => hooks.event({
  event: { type: "session.idle", properties: { sessionID: RUN } },
}));
lines.close();
