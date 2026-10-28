// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// These tests follow herdr's src/integration/assets/opencode/herdr-tui-session.test.ts at master
// 0d5d6f1. They use node:test and node:assert instead of bun:test, and the plugin's
// `node:child_process` is the fake beside this file instead of a mocked `node:net`, so a run of
// taw is what a request over herdr's socket was (see fake-child-process.mjs). A run of taw
// starts at once, so the report that was in flight when the route changed is a run that has
// started and will happen; what the plugin guarantees, and what the three tests that hold a
// connection in herdr's file check here, is that nothing queued behind it goes out once it is
// stale. herdr's test of a connection that never completes tests its own 500 ms timer; here the
// run is given a timeout and node ends it, which is checked as an option of the run.
//
// Run by tests/integration_opencode_plugins.rs, with $TAW_PLUGIN the installed TUI plugin and
// $TAW_BIN_EXPECTED the taw path it was rendered with.

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { pathToFileURL } from "node:url";

import { control, requests } from "./fake-child-process.mjs";

const pluginUrl = pathToFileURL(process.env.TAW_PLUGIN).href;
const activeDisposers = [];
let importCounter = 0;

beforeEach(() => {
  requests.length = 0;
  control.hold = false;
  control.fail = false;
  control.held.length = 0;
  control.waiters.length = 0;
  control.stateWaiters.length = 0;
  process.env.TMUX_PANE = "%1";
});

afterEach(() => {
  for (const dispose of activeDisposers.splice(0)) {
    dispose();
  }
});

async function loadPlugin() {
  importCounter += 1;
  const module = await import(`${pluginUrl}?test=${importCounter}`);
  return module.default;
}

function fakeApi() {
  const sessions = new Map();
  const statuses = {};
  const permissions = [];
  const questions = [];
  const messages = new Map();
  const listeners = new Map();
  const calls = [];
  let current = { name: "home" };
  let dispose;
  activeDisposers.push(() => dispose?.());

  return {
    statuses, permissions, questions, messages, listeners, calls,
    emit(type, properties) {
      for (const receive of listeners.get(type) ?? []) receive({ type, properties });
    },
    api: {
      client: {
        session: {
          async get({ sessionID }) {
            calls.push(`get:${sessionID}`);
            const data = sessions.get(sessionID);
            if (!data) throw new Error("session not found");
            return { data };
          },
          async status() { calls.push("status"); return { data: { ...statuses } }; },
          async message({ messageID }) {
            calls.push(`message:${messageID}`);
            const data = messages.get(messageID);
            if (!data) throw new Error("message unavailable");
            return { data };
          },
        },
        permission: { async list() { return { data: [...permissions] }; } },
        question: { async list() { return { data: [...questions] }; } },
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
          return current;
        },
      },
      state: {
        session: {
          get(sessionID) {
            return sessions.get(sessionID);
          },
        },
      },
      lifecycle: {
        onDispose(handler) {
          dispose = handler;
          return () => {};
        },
      },
    },
    addSession(session) {
      sessions.set(session.id, session);
    },
    select(sessionID) {
      current = { name: "session", params: { sessionID } };
    },
    home() { current = { name: "home" }; },
    dispose() {
      dispose?.();
    },
  };
}

function waitForNextRequest() {
  return new Promise((resolve) => control.waiters.push(resolve));
}

function waitForStateReport() {
  return new Promise((resolve) => control.stateWaiters.push(resolve));
}

function requestParam(request, name) {
  return request?.params?.[name];
}

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

test("reports a root session when only the local route changes", async () => {
  const plugin = await loadPlugin();
  const tui = fakeApi();
  tui.addSession({ id: "session-a" });
  await plugin.tui(tui.api);

  const dispatched = waitForNextRequest();
  tui.select("session-a");
  await dispatched;

  assert.equal(requests.length, 1);
  assert.equal(requestParam(requests[0], "agent_session_id"), "session-a");
  assert.equal(requestParam(requests[0], "session_start_source"), "select");
  assert.equal(requestParam(requests[0], "seq"), undefined);
});

test("asks taw to report the selection and the state with the arguments its verbs take", async () => {
  const plugin = await loadPlugin();
  const tui = fakeApi();
  tui.addSession({ id: "session-a" });
  tui.select("session-a");
  const dispatched = waitForNextRequest();
  await plugin.tui(tui.api);
  await dispatched;
  await flushReports();

  const selection = requests.find((request) => requestParam(request, "state") === undefined);
  assert.deepEqual(selection.args, [
    "pane", "report-agent-session", "%1",
    "--source", "integration:opencode",
    "--agent", "opencode",
    "--agent-session-id", "session-a",
    "--session-start-source", "select",
  ]);
  const state = requests.find((request) => requestParam(request, "state") !== undefined);
  assert.equal(state.args[0], "pane");
  assert.equal(state.args[1], "report-agent");
  assert.equal(state.args[2], "%1");
  for (const [flag, value] of [
    ["--source", "integration:opencode"],
    ["--agent", "opencode"],
    ["--state", "idle"],
    ["--agent-session-id", "session-a"],
  ]) {
    assert.equal(state.args[state.args.indexOf(flag) + 1], value, flag);
  }
  assert.match(state.args[state.args.indexOf("--seq") + 1], /^\d+$/);
});

test("says nothing outside tmux", async () => {
  delete process.env.TMUX_PANE;
  const plugin = await loadPlugin();
  const tui = fakeApi();
  tui.addSession({ id: "session-a" });
  tui.select("session-a");
  await plugin.tui(tui.api);
  await wait(125);
  assert.equal(requests.length, 0);
  assert.equal(tui.listeners.size, 0, "it subscribes to nothing");
});

test("retries an initial selection while taw detects the process", async () => {
  const plugin = await loadPlugin();
  const tui = fakeApi();
  tui.addSession({ id: "session-a" });
  tui.select("session-a");

  await plugin.tui(tui.api);
  await wait(125);

  const selections = requests.filter((request) => requestParam(request, "state") === undefined);
  assert.ok(selections.length >= 2);
  assert.ok(selections.every((request) => requestParam(request, "agent_session_id") === "session-a"));
});

test("does not report root sessions not selected by this TUI", async () => {
  const plugin = await loadPlugin();
  const tui = fakeApi();
  tui.addSession({ id: "session-a" });
  tui.addSession({ id: "session-b" });
  tui.select("session-a");
  await plugin.tui(tui.api);

  await wait(125);

  assert.ok(requests.length > 0);
  assert.ok(requests.every((request) => requestParam(request, "agent_session_id") === "session-a"));
});

test("does not replace the root session with a selected child session", async () => {
  const plugin = await loadPlugin();
  const tui = fakeApi();
  tui.addSession({ id: "root-session" });
  tui.addSession({ id: "child-session", parentID: "root-session" });
  tui.select("root-session");
  await plugin.tui(tui.api);
  await flushReports();

  tui.select("child-session");
  await wait(125);

  assert.ok(requests.length > 0);
  assert.ok(requests.every((r) => requestParam(r, "agent_session_id") === "root-session"));
});

test("stops route polling when the TUI plugin is disposed", async () => {
  const plugin = await loadPlugin();
  const tui = fakeApi();
  tui.addSession({ id: "session-a" });
  await plugin.tui(tui.api);
  tui.dispose();
  tui.select("session-a");

  await wait(125);

  assert.equal(requests.length, 0);
});

function v2Api() {
  const sessions = new Map([
    ["a", { id: "a" }],
    ["b", { id: "b" }],
    ["child", { id: "child", parentID: "a" }],
  ]);
  let route = { type: "session", sessionID: "a" };
  const listeners = new Set();
  const permissions = new Map();
  const forms = new Map();
  // How many times the plugin has read each session's permissions: once for each pass of its
  // reconciliation, which a test can wait for.
  const permissionReads = new Map();
  return {
    api: {
      ui: { router: { current: () => route } },
      data: {
        session: {
          get: (id) => sessions.get(id),
          family: () => [...sessions.keys()],
          status: () => "idle",
          permission: {
            list: (id) => {
              permissionReads.set(id, (permissionReads.get(id) ?? 0) + 1);
              return permissions.get(id);
            },
          },
          form: { list: (id) => forms.get(id) },
        },
        listen: (handler) => {
          listeners.add(handler);
          return () => listeners.delete(handler);
        },
      },
    },
    select(sessionID) { route = { type: "session", sessionID }; },
    home() { route = { type: "home", sessionID: "" }; },
    emit(type, data) {
      for (const listener of listeners) listener({ details: { type, data } });
    },
    listeners,
    sessions,
    permissions,
    forms,
    permissionReads: (id) => permissionReads.get(id) ?? 0,
  };
}

const flushReports = () => new Promise((resolve) => setTimeout(resolve, 10));

// Waits until the state the plugin last reported is `state`, report by report: a report the plugin
// queued before the change the test made (a selection retry publishing the state it last knew)
// may come first. Fails once `ms` have gone by without it.
async function waitForState(state, ms = 5_000) {
  const deadline = Date.now() + ms;
  while (states().at(-1) !== state) {
    const left = deadline - Date.now();
    if (left <= 0) {
      assert.fail(`the plugin last reported ${states().at(-1)}, not ${state}`);
    }
    await Promise.race([waitForStateReport(), wait(left)]);
  }
}

// Waits until `ready()` holds, looking every few milliseconds, for at most `ms`.
async function until(ready, what, ms = 5_000) {
  const deadline = Date.now() + ms;
  while (!ready()) {
    if (Date.now() > deadline) assert.fail(`never: ${what}`);
    await wait(5);
  }
}
const states = () => requests.filter((r) => requestParam(r, "state") !== undefined)
  .map((r) => requestParam(r, "state"));

function familyApi() {
  const tui = fakeApi();
  tui.addSession({ id: "root" });
  tui.addSession({ id: "child", parentID: "root" });
  tui.addSession({ id: "sibling", parentID: "root" });
  tui.addSession({ id: "grandchild", parentID: "child" });
  tui.addSession({ id: "other" });
  tui.select("root");
  return tui;
}

test("V1 hydrates active descendants and keeps working until the whole family settles", async () => {
  const tui = familyApi();
  tui.statuses.child = { type: "busy" };
  tui.statuses.grandchild = { type: "retry" };
  tui.statuses.other = { type: "busy" };
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  assert.equal(states().at(-1), "working");
  tui.emit("session.status", { sessionID: "child", status: { type: "idle" } });
  await flushReports();
  assert.equal(states().at(-1), "working");
  tui.emit("session.status", { sessionID: "grandchild", status: { type: "idle" } });
  await flushReports();
  assert.equal(states().at(-1), "idle");
  assert.ok(requests.every((r) => requestParam(r, "agent_session_id") === "root"));
});

test("V1 retains sibling blockers and cancellation does not finish an active parent", async () => {
  const tui = familyApi();
  tui.statuses.root = { type: "busy" };
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  tui.emit("permission.asked", { id: "p", sessionID: "child" });
  tui.emit("question.asked", { id: "q", sessionID: "sibling", tool: { messageID: "m", callID: "call" } });
  await flushReports();
  assert.equal(states().at(-1), "blocked");
  tui.emit("permission.replied", { requestID: "p", sessionID: "child" });
  await flushReports();
  assert.equal(states().at(-1), "blocked");
  tui.emit("session.idle", { sessionID: "sibling" });
  await flushReports();
  assert.equal(states().at(-1), "blocked");
  tui.emit("message.part.updated", { part: {
    type: "tool", sessionID: "sibling", messageID: "m", callID: "call", state: { status: "error" },
  } });
  await flushReports();
  assert.equal(states().at(-1), "working");
  tui.emit("session.idle", { sessionID: "root" });
  await flushReports();
  assert.equal(states().at(-1), "idle");
});

test("V1 hydration rejects aborted tool requests even if their session is busy again", async () => {
  for (const kind of ["permissions", "questions"]) {
    const tui = familyApi();
    tui.statuses.child = { type: "busy" };
    tui[kind].push({ id: "stale", sessionID: "child", tool: { messageID: "m", callID: "call" } });
    tui.messages.set("m", { info: { error: { name: "MessageAbortedError" } }, parts: [
      { type: "tool", callID: "call", state: { status: "error" } },
    ] });
    await (await loadPlugin()).tui(tui.api);
    await flushReports();
    assert.equal(states().at(-1), "working");
    tui.dispose();
  }
});

test("V1 validates pending tools instead of assuming an idle owner has no requests", async () => {
  const tui = familyApi();
  tui.permissions.push({ id: "pending", sessionID: "child", tool: { messageID: "m", callID: "call" } });
  tui.messages.set("m", { parts: [{ type: "tool", callID: "call", state: { status: "running" } }] });
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  assert.equal(states().at(-1), "blocked");
  tui.emit("message.part.updated", { part: {
    type: "tool", sessionID: "child", messageID: "m", callID: "call", state: { status: "error" },
  } });
  await flushReports();
  assert.equal(states().at(-1), "idle");
});

test("V1 replays completion and replies over a late initial snapshot", async () => {
  const tui = familyApi();
  let resolveStatus;
  tui.api.client.session.status = () => new Promise((resolve) => { resolveStatus = resolve; });
  tui.permissions.push({ id: "p", sessionID: "child" });
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  assert.ok(!states().includes("idle"));
  tui.emit("session.idle", { sessionID: "child" });
  tui.emit("permission.replied", { sessionID: "child", requestID: "p" });
  resolveStatus({ data: { child: { type: "busy" } } });
  await flushReports();
  assert.equal(states().at(-1), "idle");
});

test("V1 retains replies received between failed hydration and its retry", async () => {
  const tui = familyApi();
  tui.permissions.push({ id: "p", sessionID: "child" });
  const status = tui.api.client.session.status;
  tui.api.client.session.status = async () => {
    tui.api.client.session.status = status;
    throw new Error("temporarily unavailable");
  };
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  tui.emit("permission.replied", { sessionID: "child", requestID: "p" });
  await wait(650);
  assert.equal(states().at(-1), "idle");
  assert.ok(!states().includes("blocked"));
});

test("V1 deleted sessions cannot return through a late hydration snapshot", async () => {
  const tui = familyApi();
  let resolveStatus;
  tui.api.client.session.status = () => new Promise((resolve) => { resolveStatus = resolve; });
  tui.permissions.push({ id: "p", sessionID: "child" });
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  tui.emit("session.deleted", { info: { id: "child", parentID: "root" } });
  resolveStatus({ data: { child: { type: "busy" } } });
  await flushReports();
  assert.equal(states().at(-1), "idle");
});

test("V1 ignores stale hydration across A/B/A route changes and disposal", async () => {
  const tui = familyApi();
  let resolveOld;
  const status = tui.api.client.session.status;
  tui.api.client.session.status = () => {
    tui.api.client.session.status = status;
    return new Promise((resolve) => { resolveOld = resolve; });
  };
  await (await loadPlugin()).tui(tui.api);
  tui.select("other");
  tui.emit("session.updated", { info: { id: "other" } });
  await flushReports();
  tui.select("root");
  tui.emit("session.updated", { info: { id: "root" } });
  await flushReports();
  requests.length = 0;
  resolveOld({ data: { root: { type: "busy" } } });
  await flushReports();
  assert.ok(!states().includes("working"));
  tui.dispose();
  assert.ok([...tui.listeners.values()].every((set) => set.size === 0));
});

test("V1 a directly attached child owns its descendants, not its parent or siblings", async () => {
  const tui = familyApi();
  tui.select("child");
  tui.statuses.root = { type: "busy" };
  tui.statuses.sibling = { type: "busy" };
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  assert.equal(states().at(-1), "idle");
  tui.emit("session.status", { sessionID: "grandchild", status: { type: "busy" } });
  await flushReports();
  assert.equal(states().at(-1), "working");
  tui.select("grandchild");
  tui.emit("session.updated", { info: { id: "grandchild", parentID: "child" } });
  await flushReports();
  assert.ok(requests.every((r) => requestParam(r, "agent_session_id") === "child"));
});

test("V1 a terminal tool event during hydration cannot revive a cancelled blocker", async () => {
  const tui = familyApi();
  tui.questions.push({ id: "q", sessionID: "child", tool: { messageID: "m", callID: "call" } });
  let resolveMessage;
  tui.api.client.session.message = () => new Promise((resolve) => { resolveMessage = resolve; });
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  tui.emit("message.part.updated", { part: {
    type: "tool", sessionID: "child", messageID: "m", callID: "call", state: { status: "error" },
  } });
  resolveMessage({ data: { parts: [{ type: "tool", callID: "call", state: { status: "running" } }] } });
  await flushReports();
  assert.equal(states().at(-1), "idle");
});

test("V1 unknown tool evidence remains blocked and retries failed message reads", async () => {
  const tui = familyApi();
  tui.permissions.push({ id: "p", sessionID: "child", tool: { messageID: "m", callID: "call" } });
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  assert.equal(states().at(-1), "blocked");
  tui.messages.set("m", { parts: [{ type: "tool", callID: "call", state: { status: "error" } }] });
  await wait(650);
  assert.equal(states().at(-1), "idle");
});

test("V1 request validation matches the exact tool and deduplicates message reads", async () => {
  const tui = familyApi();
  tui.permissions.push({ id: "p", sessionID: "child", tool: { messageID: "m", callID: "running" } });
  tui.questions.push({ id: "q", sessionID: "child", tool: { messageID: "m", callID: "finished" } });
  tui.messages.set("m", { parts: [
    { type: "tool", callID: "running", state: { status: "running" } },
    { type: "tool", callID: "finished", state: { status: "completed" } },
  ] });
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  assert.equal(states().at(-1), "blocked");
  assert.equal(tui.calls.filter((call) => call === "message:m").length, 1);
  tui.emit("permission.replied", { sessionID: "child", requestID: "p" });
  await flushReports();
  assert.equal(states().at(-1), "idle");
});

test("V1 reselecting an aborted request does not resurrect it or poll completed tools", async () => {
  const tui = familyApi();
  tui.permissions.push({ id: "p", sessionID: "child", tool: { messageID: "m", callID: "call" } });
  tui.messages.set("m", { parts: [{ type: "tool", callID: "call", state: { status: "error" } }] });
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  tui.select("other");
  tui.emit("session.updated", { info: { id: "other" } });
  await flushReports();
  tui.select("root");
  tui.emit("session.updated", { info: { id: "root" } });
  await flushReports();
  assert.ok(!states().includes("blocked"));
  const reads = tui.calls.length;
  await wait(650);
  assert.equal(tui.calls.length, reads);
});

test("V1 home and selected-session deletion settle authority and retry a dropped idle report", async () => {
  for (const action of ["home", "delete"]) {
    const tui = familyApi();
    tui.statuses.root = { type: "busy" };
    await (await loadPlugin()).tui(tui.api);
    await flushReports();
    assert.equal(states().at(-1), "working");
    control.fail = true;
    if (action === "home") {
      tui.home();
      tui.emit("session.updated", { info: { id: "root" } });
    } else tui.emit("session.deleted", { info: { id: "root" } });
    await flushReports();
    control.fail = false;
    await wait(650);
    assert.equal(states().at(-1), "idle");
    tui.dispose();
  }
});

// herdr holds a connection while the route changes and shows that the report it carried is
// never written. A run of taw has started when it is recorded, so the settlement of the old
// selection is already out; what is held back is everything queued behind it, and none of that
// reaches taw once the selection has moved on.
test("V1 a delayed home settlement cannot overwrite the next selected session", async () => {
  const tui = familyApi();
  tui.statuses.root = { type: "busy" };
  await (await loadPlugin()).tui(tui.api);
  await flushReports();
  requests.length = 0;
  control.hold = true;
  tui.home();
  tui.emit("session.updated", { info: { id: "root" } });
  await flushReports();
  assert.equal(control.held.length, 1);
  assert.equal(requestParam(requests[0], "agent_session_id"), "root");
  assert.equal(requestParam(requests[0], "state"), "idle");
  tui.select("other");
  tui.emit("session.updated", { info: { id: "other" } });
  await flushReports();
  control.hold = false;
  for (const release of control.held.splice(0)) release();
  await flushReports();
  assert.ok(requests.length > 1);
  assert.ok(requests.slice(1).every((r) => requestParam(r, "agent_session_id") === "other"));
  assert.equal(states().at(-1), "idle");
});

test("V2 ignores events without data", async () => {
  const plugin = await loadPlugin();
  const tui = v2Api();
  const dispose = await plugin.setup(tui.api);
  activeDisposers.push(dispose);
  await flushReports();
  requests.length = 0;
  assert.doesNotThrow(() => tui.emit("legacy.event"));
  tui.emit("session.execution.started", { sessionID: "a" });
  await flushReports();
  assert.deepEqual(states(), ["working"]);
});

test("V2 completes and interrupts without legacy idle events", async () => {
  for (const terminal of ["succeeded", "interrupted", "failed"]) {
    const plugin = await loadPlugin();
    const tui = v2Api();
    const dispose = await plugin.setup(tui.api);
    activeDisposers.push(dispose);
    await flushReports();
    requests.length = 0;
    tui.emit("session.execution.started", { sessionID: "a" });
    tui.emit(`session.execution.${terminal}`, { sessionID: "a" });
    await flushReports();
    assert.deepEqual(states(), ["working", terminal === "failed" ? "blocked" : "idle"]);
    dispose();
  }
});

test("V2 aggregates root and child blockers and ignores other roots and child completion", async () => {
  const plugin = await loadPlugin();
  const tui = v2Api();
  const dispose = await plugin.setup(tui.api);
  activeDisposers.push(dispose);
  await flushReports();
  requests.length = 0;
  tui.emit("session.execution.started", { sessionID: "a" });
  tui.emit("permission.asked", { sessionID: "a", id: "permission-a" });
  tui.emit("form.created", { form: { sessionID: "child", id: "form-child" } });
  tui.emit("permission.replied", { sessionID: "a", requestID: "permission-a" });
  tui.emit("session.execution.succeeded", { sessionID: "child" });
  tui.emit("session.execution.started", { sessionID: "b" });
  tui.emit("permission.asked", { sessionID: "b", id: "other" });
  await flushReports();
  assert.equal(states().at(-1), "blocked");
  assert.ok(requests.every((r) => requestParam(r, "agent_session_id") === "a"));
  tui.emit("form.cancelled", { sessionID: "child", id: "form-child" });
  tui.emit("session.execution.succeeded", { sessionID: "a" });
  await flushReports();
  assert.deepEqual(states().slice(-2), ["working", "idle"]);
});

test("V2 discards queued reports after selection changes and stops on disposal", async () => {
  const plugin = await loadPlugin();
  const tui = v2Api();
  const dispose = await plugin.setup(tui.api);
  activeDisposers.push(dispose);
  await flushReports();
  requests.length = 0;
  tui.emit("session.execution.started", { sessionID: "a" });
  tui.select("b");
  tui.emit("session.execution.started", { sessionID: "b" });
  await flushReports();
  assert.ok(requests.every((r) => requestParam(r, "agent_session_id") === "b"));
  requests.length = 0;
  tui.emit("session.execution.succeeded", { sessionID: "b" });
  tui.home();
  await flushReports();
  assert.equal(requests.length, 0);
  dispose();
  assert.equal(tui.listeners.size, 0);
  tui.select("a");
  await wait(250);
  assert.equal(requests.length, 0);
});

// Each step waits for the state it checks rather than for the next report or a stretch of time: a
// selection retry that comes due as the reply arrives publishes the blocker the plugin still knows
// first, and the reply's `idle` follows it (seen under load as `blocked` where `idle` was wanted).
test("V2 reconciles late blocker hydration without reviving an already-replied request", async () => {
  const plugin = await loadPlugin();
  const tui = v2Api();
  const dispose = await plugin.setup(tui.api);
  activeDisposers.push(dispose);
  await flushReports();
  tui.permissions.set("child", [{ id: "late" }]);
  await waitForState("blocked");
  tui.emit("permission.replied", { sessionID: "child", requestID: "late" });
  await waitForState("idle");
  // The list still holds the request the plugin was told was replied to: the next two passes of its
  // reconciliation read it, and the reply stands.
  const read = tui.permissionReads("child");
  await until(
    () => tui.permissionReads("child") >= read + 2,
    "two more reads of the child's permissions",
  );
  assert.equal(states().at(-1), "idle", "the replied request was revived");
  tui.permissions.set("child", []);
  tui.forms.set("child", [{ id: "second" }]);
  await waitForState("blocked");
  tui.sessions.delete("child");
  tui.emit("session.deleted", { sessionID: "child" });
  await waitForState("idle");
});

// herdr: a connection that comes up after a disposal or a session switch writes nothing. Here
// the first run of taw is held while that happens, and the report queued behind it is dropped
// when it comes to the front, since neither the plugin nor the selection is what it was for.
test("V2 never runs taw for a queued report after disposal or a session switch", async () => {
  for (const action of ["dispose", "switch"]) {
    const plugin = await loadPlugin();
    const tui = v2Api();
    control.hold = true;
    requests.length = 0;
    const dispose = await plugin.setup(tui.api);
    activeDisposers.push(dispose);
    await flushReports();
    assert.equal(control.held.length, 1, "the selection report is running");
    assert.equal(requests.length, 1);
    assert.equal(requestParam(requests[0], "state"), undefined);
    if (action === "dispose") dispose();
    else tui.select("b");
    control.hold = false;
    for (const release of control.held.splice(0)) release();
    await flushReports();
    if (action === "dispose") {
      assert.equal(requests.length, 1, "nothing was run after the disposal");
    } else {
      assert.ok(
        requests.slice(1).every((r) => requestParam(r, "agent_session_id") === "b"),
        "nothing more was run for the session that was left",
      );
    }
    dispose();
  }
});

test("V2 gives every run of taw a timeout, so one that never completes is ended and the queue moves on", async () => {
  const plugin = await loadPlugin();
  const tui = v2Api();
  const dispose = await plugin.setup(tui.api);
  activeDisposers.push(dispose);
  await flushReports();
  assert.ok(requests.length > 0);
  assert.ok(requests.every((request) => {
    assert.equal(typeof request.options.timeout, "number");
    return request.options.timeout > 0 && request.options.timeout <= 10_000;
  }));
});

test("V2 resends the latest state after a failed delivery", async () => {
  const plugin = await loadPlugin();
  const tui = v2Api();
  const dispose = await plugin.setup(tui.api);
  activeDisposers.push(dispose);
  await flushReports();
  // Exhaust the selection retry schedule so only the event report remains.
  await wait(1_600);
  requests.length = 0;
  tui.emit("session.execution.started", { sessionID: "a" });
  await flushReports();
  control.fail = true;
  tui.emit("session.execution.succeeded", { sessionID: "a" });
  const resend = waitForStateReport();
  await wait(700);
  control.fail = false;
  await resend;
  assert.equal(states().at(-1), "idle");
  dispose();
});

test("the default export is the one plugin object both versions load, named for taw", async () => {
  const plugin = await loadPlugin();
  assert.equal(plugin.id, "taw.opencode.session-selection");
  assert.equal(typeof plugin.tui, "function");
  assert.equal(typeof plugin.setup, "function");
});
