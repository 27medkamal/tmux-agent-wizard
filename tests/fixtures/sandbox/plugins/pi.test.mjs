// Pi's and OMP's plugins inside a sandbox (taw's own), run under node's type
// stripping by tests/integration_sandbox_plugins.rs.
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { beforeEach, test } from "node:test";
import { pathToFileURL } from "node:url";

import { requests } from "../../opencode/fake-child-process.mjs";

const AGENT = process.env.TAW_AGENT;
const SESSION_FILE = "/home/agent/.pi/sessions/2026-10-02.jsonl";
// Each test after the first writes into a folder of its own inside the one the runner made, with
// a fresh copy of the plugin (it keeps the session it saw at module level).
const base = dirname(process.env.TAW_REPORT_FILE);
let copies = 0;
let first = true;

beforeEach(() => {
  requests.length = 0;
  delete process.env.TMUX_PANE;
  delete process.env.TAW_REPORT_AGENT;
  delete process.env.OMPCODE;
  if (first) {
    first = false;
    return;
  }
  process.env.TAW_REPORT_FILE = join(mkdtempSync(join(base, "case-")), "session.json");
});

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const report = () => JSON.parse(readFileSync(process.env.TAW_REPORT_FILE, "utf8"));
const listing = () => readdirSync(dirname(process.env.TAW_REPORT_FILE));

/** A fresh copy of the plugin, loaded: its handlers by event name. */
async function loaded() {
  copies += 1;
  const { default: plugin } = await import(`${pathToFileURL(process.env.TAW_PLUGIN).href}?copy=${copies}`);
  const handlers = {};
  plugin({ on: (name, handler) => { handlers[name] = handler; }, events: { on() {} } });
  return handlers;
}

function context({ file = SESSION_FILE, idle = true } = {}) {
  return {
    mode: "tui",
    hasUI: true,
    isIdle: () => idle,
    sessionManager: { getSessionFile: () => file, getSessionId: () => "pi-id-1" },
  };
}

test("the session file goes to the report file, and taw is never run", async () => {
  delete process.env.TMUX_PANE;
  const { default: plugin } = await import(pathToFileURL(process.env.TAW_PLUGIN).href);
  const handlers = {};
  plugin({ on: (name, handler) => { handlers[name] = handler; }, events: { on() {} } });
  // Pi takes a TUI by its mode, OMP by hasUI: the one context answers both.
  const ctx = {
    mode: "tui",
    hasUI: true,
    isIdle: () => true,
    sessionManager: { getSessionFile: () => "/home/agent/.pi/sessions/2026-10-02.jsonl", getSessionId: () => "pi-id-1" },
  };
  await handlers.session_start({ reason: "startup" }, ctx);

  assert.deepEqual(JSON.parse(readFileSync(process.env.TAW_REPORT_FILE, "utf8")), {
    hook_event_name: "SessionStart",
    transcript_path: "/home/agent/.pi/sessions/2026-10-02.jsonl",
    source: "startup",
    taw_agent: process.env.TAW_AGENT,
  });
  assert.equal(requests.length, 0);
});

test("with no session file the id goes to the report file", async () => {
  const handlers = await loaded();
  await handlers.session_start({ reason: "startup" }, context({ file: null }));
  assert.deepEqual(report(), {
    hook_event_name: "SessionStart", session_id: "pi-id-1", source: "startup", taw_agent: AGENT,
  });
  assert.equal(requests.length, 0);
});

test("the start's reason is what the plugin sends outside: Pi's own, OMP's startup, and resume on OMP's switch", async () => {
  const handlers = await loaded();
  await handlers.session_start({ reason: "fork" }, context());
  assert.equal(report().source, AGENT === "pi" ? "fork" : "startup");
  if (AGENT === "omp") {
    await handlers.session_switch({}, context());
    assert.equal(report().source, "resume");
    await handlers.session_switch({ reason: "new" }, context());
    assert.equal(report().source, "new");
  }
  assert.equal(requests.length, 0);
});

test("a state event leaves only the report, with what the plugin sends outside, and taw is never run", async () => {
  const handlers = await loaded();
  await handlers.session_start({ reason: "startup" }, context());
  await handlers.agent_start({}, context({ idle: false }));
  await handlers.agent_settled?.({}, context());
  await wait(50);

  assert.deepEqual(listing(), ["session.json"]);
  // Outside, Pi's agent_start tells taw the session with no start source, OMP's with startup.
  const expected = { hook_event_name: "SessionStart", transcript_path: SESSION_FILE, taw_agent: AGENT };
  if (AGENT === "omp") expected.source = "startup";
  assert.deepEqual(report(), expected);
  assert.equal(requests.length, 0);
});

test("the report file is taken first: with TMUX_PANE set as well, taw is still never run", async () => {
  process.env.TMUX_PANE = "%1";
  const handlers = await loaded();
  await handlers.session_start({ reason: "startup" }, context({ idle: false }));
  await handlers.agent_start({}, context({ idle: false }));
  await wait(50);

  assert.equal(report().transcript_path, SESSION_FILE);
  assert.deepEqual(listing(), ["session.json"]);
  assert.equal(requests.length, 0);
});

test("with TAW_REPORT_AGENT naming another agent nothing is written, and its own name writes", async () => {
  process.env.TAW_REPORT_AGENT = "claude";
  const handlers = await loaded();
  await handlers.session_start({ reason: "startup" }, context());
  assert.equal(existsSync(process.env.TAW_REPORT_FILE), false);
  assert.deepEqual(listing(), []);

  process.env.TAW_REPORT_AGENT = AGENT;
  await handlers.session_start({ reason: "startup" }, context());
  assert.equal(report().transcript_path, SESSION_FILE);
  assert.equal(requests.length, 0);
});

test("a gone report folder ends quietly and leaves nothing behind", async () => {
  process.env.TAW_REPORT_FILE = join(base, "gone", "session.json");
  const handlers = await loaded();
  await handlers.session_start({ reason: "startup" }, context());
  assert.equal(existsSync(join(base, "gone")), false);
  assert.equal(requests.length, 0);
});

test("outside a sandbox it runs as today: with TMUX_PANE alone, taw is told the session", async () => {
  const report_file = process.env.TAW_REPORT_FILE;
  delete process.env.TAW_REPORT_FILE;
  process.env.TMUX_PANE = "%1";
  try {
    const handlers = await loaded();
    await handlers.session_start({ reason: "startup" }, context());
    await wait(50);
    assert.ok(requests.length >= 1);
    assert.equal(requests[0].method, "pane.report_agent_session");
    assert.ok(requests[0].args.includes(SESSION_FILE));
  } finally {
    process.env.TAW_REPORT_FILE = report_file;
  }
});

test("with neither TMUX_PANE nor TAW_REPORT_FILE it listens to nothing", async () => {
  const report_file = process.env.TAW_REPORT_FILE;
  delete process.env.TAW_REPORT_FILE;
  try {
    assert.deepEqual(await loaded(), {});
  } finally {
    process.env.TAW_REPORT_FILE = report_file;
  }
});

if (AGENT === "omp") {
  test("OMP's nested run (OMPCODE=1) writes nothing inside either", async () => {
    process.env.OMPCODE = "1";
    assert.deepEqual(await loaded(), {});
    assert.equal(existsSync(process.env.TAW_REPORT_FILE), false);
  });
}
