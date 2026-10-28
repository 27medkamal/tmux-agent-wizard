// The TUI plugin inside a sandbox (taw's own): with TAW_REPORT_FILE set and no
// TMUX_PANE, a selection is written to that file as a SessionStart, beside it and renamed into
// place; a state writes nothing; taw is never run.
//
// Run by tests/integration_opencode_plugins.rs, with $TAW_PLUGIN the installed TUI plugin.

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { pathToFileURL } from "node:url";

import { requests } from "./fake-child-process.mjs";

const pluginUrl = pathToFileURL(process.env.TAW_PLUGIN).href;
const disposers = [];
let folder;
let importCounter = 0;

beforeEach(() => {
  requests.length = 0;
  delete process.env.TMUX_PANE;
  delete process.env.TAW_REPORT_AGENT;
  folder = mkdtempSync(join(tmpdir(), "taw-report-"));
  process.env.TAW_REPORT_FILE = join(folder, "session.json");
});

afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose?.();
  delete process.env.TAW_REPORT_FILE;
  delete process.env.TMUX_PANE;
  rmSync(folder, { recursive: true, force: true });
});

async function loadPlugin() {
  importCounter += 1;
  return (await import(`${pluginUrl}?sandbox=${importCounter}`)).default;
}

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const report = () => JSON.parse(readFileSync(process.env.TAW_REPORT_FILE, "utf8"));
const start = (id) => ({ hook_event_name: "SessionStart", session_id: id, source: "select", taw_agent: "opencode" });

async function written() {
  for (let i = 0; i < 100 && !existsSync(process.env.TAW_REPORT_FILE); i += 1) await wait(20);
  return report();
}

/** As much of OpenCode 1's TUI api as a selection and a state need. */
function v1Api(sessionID) {
  const listeners = new Map();
  let dispose;
  disposers.push(() => dispose?.());
  const none = async () => ({ data: [] });
  return {
    emit(type, properties) {
      for (const receive of listeners.get(type) ?? []) receive({ type, properties });
    },
    api: {
      client: {
        session: {
          async get() { return { data: { id: sessionID } }; },
          async status() { return { data: {} }; },
          async message() { throw new Error("no messages"); },
        },
        permission: { list: none },
        question: { list: none },
      },
      event: {
        on(type, receive) {
          if (!listeners.has(type)) listeners.set(type, new Set());
          listeners.get(type).add(receive);
          return () => listeners.get(type).delete(receive);
        },
      },
      route: { current: { name: "session", params: { sessionID } } },
      state: { session: { get: (id) => (id === sessionID ? { id } : undefined) } },
      lifecycle: { onDispose(handler) { dispose = handler; return () => {}; } },
    },
  };
}

/** As much of OpenCode 2's TUI api as a selection and a state need. */
function v2Api(sessionID) {
  const listeners = new Set();
  return {
    emit(type, data) {
      for (const listener of listeners) listener({ details: { type, data } });
    },
    api: {
      ui: { router: { current: () => ({ type: "session", sessionID }) } },
      data: {
        session: {
          get: (id) => (id === sessionID ? { id } : undefined),
          family: () => [sessionID],
          status: () => "idle",
          permission: { list: () => [] },
          form: { list: () => [] },
        },
        listen: (handler) => { listeners.add(handler); return () => listeners.delete(handler); },
      },
    },
  };
}

test("OpenCode 1 writes the selected session to the report file and runs no taw", async () => {
  const plugin = await loadPlugin();
  await plugin.tui(v1Api("ses_7f3a91c2bffe4Qz1Wm").api);

  assert.deepEqual(await written(), start("ses_7f3a91c2bffe4Qz1Wm"));
  assert.deepEqual(readdirSync(folder), ["session.json"], "nothing is left beside it");
  assert.equal(requests.length, 0, "taw is never run");
});

test("OpenCode 1 writes nothing for a state", async () => {
  const plugin = await loadPlugin();
  const tui = v1Api("ses_a");
  await plugin.tui(tui.api);
  await written();

  tui.emit("session.status", { sessionID: "ses_a", status: { type: "busy" } });
  // Past the selection's own retries, at 100, 400 and 1000 ms.
  await wait(1_600);

  assert.deepEqual(report(), start("ses_a"));
  assert.equal(requests.length, 0);
});

test("OpenCode 2 writes the selected session to the report file, and nothing for a state", async () => {
  const plugin = await loadPlugin();
  const tui = v2Api("ses_b");
  disposers.push(await plugin.setup(tui.api));

  assert.deepEqual(await written(), start("ses_b"));
  tui.emit("session.execution.started", { sessionID: "ses_b" });
  await wait(1_600);
  assert.deepEqual(report(), start("ses_b"));
  assert.equal(requests.length, 0);
});

test("the report file is taken first: with TMUX_PANE set as well, taw is still never run", async () => {
  process.env.TMUX_PANE = "%9";
  const plugin = await loadPlugin();
  await plugin.tui(v1Api("ses_d").api);

  assert.deepEqual(await written(), start("ses_d"));
  await wait(125);
  assert.equal(requests.length, 0, "taw is never run");
});

test("a selection that went stale before it was written is not written", async () => {
  // The route reads as ses_old for the first two reads (setup's, and the queue's own check) and as
  // ses_new from the third on, which is the report's own check as the selection is dispatched.
  let reads = 0;
  const plugin = await loadPlugin();
  const tui = v2Api("ses_old");
  tui.api.ui.router.current = () => ({ type: "session", sessionID: ++reads <= 2 ? "ses_old" : "ses_new" });
  tui.api.data.session.get = (id) => ({ id });
  disposers.push(await plugin.setup(tui.api));

  // Before the route poll (100 ms) names the new session: nothing names the old one.
  await wait(50);
  assert.equal(existsSync(process.env.TAW_REPORT_FILE), false, "the stale selection was written");
  assert.deepEqual(await written(), start("ses_new"));
  assert.equal(requests.length, 0);
});

test("with TAW_REPORT_AGENT naming another agent nothing is written, and its own name or none writes", async () => {
  process.env.TAW_REPORT_AGENT = "claude";
  try {
    const plugin = await loadPlugin();
    await plugin.tui(v1Api("ses_e").api);
    await wait(400);
    assert.deepEqual(readdirSync(folder), [], "another agent is named");
    assert.equal(requests.length, 0);

    process.env.TAW_REPORT_AGENT = "opencode";
    const own = await loadPlugin();
    await own.tui(v1Api("ses_f").api);
    assert.deepEqual(await written(), start("ses_f"), "its own name is given");
    rmSync(process.env.TAW_REPORT_FILE);

    delete process.env.TAW_REPORT_AGENT;
    const unset = await loadPlugin();
    await unset.tui(v1Api("ses_g").api);
    assert.deepEqual(await written(), start("ses_g"), "no name is given");
    assert.equal(requests.length, 0);
  } finally {
    delete process.env.TAW_REPORT_AGENT;
  }
});

test("with neither TMUX_PANE nor TAW_REPORT_FILE it subscribes to nothing", async () => {
  delete process.env.TAW_REPORT_FILE;
  const plugin = await loadPlugin();
  await plugin.tui(v1Api("ses_c").api);
  await wait(125);

  assert.deepEqual(readdirSync(folder), []);
  assert.equal(requests.length, 0);
});
