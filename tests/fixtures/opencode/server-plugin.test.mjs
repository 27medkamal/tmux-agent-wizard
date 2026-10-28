// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// These tests follow herdr's src/integration/assets/opencode/herdr-agent-state.test.ts at master
// 0d5d6f1. They use node:test and node:assert instead of bun:test, and the plugin's
// `node:child_process` is the fake beside this file instead of a mocked `node:net`, so a run of
// taw is what a request over herdr's socket was (see fake-child-process.mjs); a report that is
// held is a run of taw that has not exited. Three tests are taw's own: `permission.updated`,
// the arguments the two verbs take, and silence outside tmux.
//
// Run by tests/integration_opencode_plugins.rs, with $TAW_PLUGIN the installed server plugin.

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { pathToFileURL } from "node:url";

import { control, requests } from "./fake-child-process.mjs";

const pluginUrl = pathToFileURL(process.env.TAW_PLUGIN).href;
const originalArgv = process.argv;
let importCounter = 0;

afterEach(() => {
  process.argv = originalArgv;
});

beforeEach(() => {
  requests.length = 0;
  control.hold = false;
  control.fail = false;
  control.held.length = 0;
  control.waiters.length = 0;
  control.stateWaiters.length = 0;
  process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
  process.env.TMUX_PANE = "%1";
});

async function loadPlugin() {
  importCounter += 1;
  const { TawAgentStatePlugin } = await import(`${pluginUrl}?test=${importCounter}`);
  return TawAgentStatePlugin();
}

function waitForNextRequest() {
  return new Promise((resolve) => control.waiters.push(resolve));
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 10));

function requestMethod(request) {
  return request?.method;
}

function requestParam(request, name) {
  return request?.params?.[name];
}

const requestState = (request) => requestParam(request, "state");
const requestSeq = (request) => requestParam(request, "seq");
const requestSessionID = (request) => requestParam(request, "agent_session_id");

test("serializes lifecycle reports", async () => {
  control.hold = true;
  const plugin = await loadPlugin();
  const firstDispatched = waitForNextRequest();
  const working = plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "busy" } },
    },
  });
  await firstDispatched;

  const secondDispatched = waitForNextRequest();
  const idle = plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "idle" } },
    },
  });
  await flush();
  assert.equal(requests.length, 1, "the second report waits for the first run of taw to exit");

  control.held.shift()();
  await secondDispatched;
  assert.equal(requests.length, 2);
  control.held.shift()();
  await Promise.all([working, idle]);

  assert.deepEqual(requests.map(requestState), ["working", "idle"]);
  const sequences = requests.map(requestSeq);
  assert.equal(typeof sequences[0], "number");
  assert.equal(sequences[1], sequences[0] + 1);
});

test("suppresses redundant same-session updates", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "busy" } },
    },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "root-session" } },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "replacement-session" } },
  });

  assert.deepEqual(requests.map(requestMethod), [
    "pane.report_agent",
    "pane.report_agent_session",
  ]);
  assert.deepEqual(requests.map(requestSessionID), ["root-session", "replacement-session"]);
});

test("does not classify server activity in another root session as a selection", async () => {
  const plugin = await loadPlugin();

  await plugin["chat.message"]({ sessionID: "visible-session" });
  await plugin["chat.message"]({ sessionID: "attached-client-session" });

  assert.deepEqual(requests.map(requestMethod), [
    "pane.report_agent",
    "pane.report_agent",
  ]);
  assert.deepEqual(requests.map(requestSessionID), [
    "visible-session",
    "attached-client-session",
  ]);
});

test("does not classify server-global root creation as a local selection", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: { type: "session.created", properties: { sessionID: "attached-session" } },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "attached-session" } },
  });
  await plugin["chat.message"]({ sessionID: "attached-session" });

  assert.deepEqual(requests.map(requestMethod), ["pane.report_agent"]);
  assert.deepEqual(requests.map(requestSessionID), ["attached-session"]);
});

test("reports retry status as working", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "retry" } },
    },
  });

  assert.deepEqual(requests.map(requestMethod), ["pane.report_agent"]);
  assert.deepEqual(requests.map(requestState), ["working"]);
  assert.deepEqual(requests.map(requestSessionID), ["root-session"]);
});

test("reports child prompts without replacing the root session", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.created",
      properties: {
        info: { id: "child-session", parentID: "root-session" },
      },
    },
  });

  for (const type of ["permission.asked", "question.asked"]) {
    await plugin.event({ event: { type, properties: { sessionID: "child-session" } } });
  }
  for (const type of ["permission.replied", "question.replied", "question.rejected"]) {
    await plugin.event({ event: { type, properties: { sessionID: "child-session" } } });
  }

  assert.deepEqual(requests.map(requestState), [
    "blocked",
    "blocked",
    "working",
    "working",
    "working",
  ]);
  assert.deepEqual(requests.map(requestSessionID), [
    "root-session",
    "root-session",
    "root-session",
    "root-session",
    "root-session",
  ]);
});

test("routes nested child prompts to their own root, not the last active root", async () => {
  const plugin = await loadPlugin();
  for (const info of [
    { id: "child-session", parentID: "root-session" },
    { id: "nested-session", parentID: "child-session" },
  ]) {
    await plugin.event({ event: { type: "session.created", properties: { info } } });
  }
  await plugin["chat.message"]({ sessionID: "other-root" });
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "nested-session" } },
  });
  await plugin.event({
    event: { type: "permission.replied", properties: { sessionID: "nested-session" } },
  });
  await plugin.event({
    event: { type: "session.idle", properties: { sessionID: "nested-session" } },
  });
  await plugin["chat.message"]({ sessionID: "nested-session" });

  assert.deepEqual(requests.map(requestState), ["working", "blocked", "working"]);
  assert.deepEqual(requests.map(requestSessionID), [
    "other-root",
    "root-session",
    "root-session",
  ]);
});

test("only local run and Mini own server lifecycle, never shared servers or TUI workers", async () => {
  for (const args of [
    ["run"], ["run", "--session", "existing"], ["--mini"], ["--mini", "--session", "existing"],
    ["--print-logs", "--log-level", "DEBUG", "run"], ["run", "--", "--attach"],
  ]) {
    process.argv = ["bun", "/$bunfs/root/src/index.js", ...args];
    assert.equal(typeof (await loadPlugin()).event, "function", args.join(" "));
  }
  for (const args of [
    [], ["--session", "existing"], ["serve"], ["web"], ["attach", "http://localhost:4096"],
    ["run", "--attach", "http://localhost:4096"], ["--mini", "--attach=http://localhost:4096"],
    ["serve", "--", "--mini"],
  ]) {
    process.argv = ["bun", "/$bunfs/root/src/index.js", ...args];
    assert.deepEqual(await loadPlugin(), {}, args.join(" "));
  }
  process.argv = ["bun", "/$bunfs/root/src/cli/tui/worker.js"];
  assert.deepEqual(await loadPlugin(), {});
  assert.equal(requests.length, 0);
});

test("dual server entrypoint keeps V1 hooks and never reports from the V2 shared server", async () => {
  const module = await import(`${pluginUrl}?test=${++importCounter}`);
  assert.equal(module.default.server, module.TawAgentStatePlugin);
  assert.equal(await module.default.setup({}), undefined);
  assert.equal(requests.length, 0);
  const hooks = await module.default.server();
  await hooks["chat.message"]({ sessionID: "v1-root" });
  assert.deepEqual(requests.map(requestState), ["working"]);
});

test("a permission asked through the older event name blocks the root and its child, as before", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: { type: "permission.updated", properties: { sessionID: "root-session" } },
  });
  await plugin.event({
    event: {
      type: "session.created",
      properties: { info: { id: "child-session", parentID: "root-session" } },
    },
  });
  await plugin.event({
    event: { type: "permission.updated", properties: { sessionID: "child-session" } },
  });

  assert.deepEqual(requests.map(requestState), ["blocked", "blocked"]);
  assert.deepEqual(requests.map(requestSessionID), ["root-session", "root-session"]);
});

test("asks taw to report with the arguments its two verbs take", async () => {
  const plugin = await loadPlugin();

  await plugin["chat.message"]({ sessionID: "root-session" });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "replacement-session" } },
  });

  const [state, session] = requests;
  assert.deepEqual(state.args.slice(0, 9), [
    "pane", "report-agent", "%1",
    "--source", "integration:opencode",
    "--agent", "opencode",
    "--seq", String(requestSeq(state)),
  ]);
  assert.deepEqual(state.args.slice(9), ["--state", "working", "--agent-session-id", "root-session"]);
  assert.deepEqual(session.args.slice(0, 3), ["pane", "report-agent-session", "%1"]);
  assert.deepEqual(session.args.slice(3, 7), [
    "--source", "integration:opencode", "--agent", "opencode",
  ]);
  assert.deepEqual(session.args.slice(9), ["--agent-session-id", "replacement-session"]);
  assert.ok(!session.args.includes("--state"));
  for (const request of requests) {
    assert.ok(request.options.timeout > 0 && request.options.timeout <= 10_000);
  }
});

test("says nothing outside tmux", async () => {
  delete process.env.TMUX_PANE;
  assert.deepEqual(await loadPlugin(), {});
  assert.equal(requests.length, 0);
});
