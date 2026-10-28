// Kilo's plugin inside a sandbox (taw's own). Run by tests/integration_sandbox_plugins.rs
// with $TAW_PLUGIN the rendered plugin and $TAW_REPORT_FILE a file in a folder of its own.
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { beforeEach, test } from "node:test";
import { pathToFileURL } from "node:url";

import { requests } from "../../opencode/fake-child-process.mjs";

// Each test writes into a folder of its own inside the one the runner made.
const base = dirname(process.env.TAW_REPORT_FILE);

beforeEach(() => {
  requests.length = 0;
  delete process.env.TMUX_PANE;
  delete process.env.TAW_REPORT_AGENT;
  process.env.TAW_REPORT_FILE = join(mkdtempSync(join(base, "case-")), "session.json");
});

const plugin = async () => (await import(pathToFileURL(process.env.TAW_PLUGIN).href)).TawAgentStatePlugin();
const report = () => JSON.parse(readFileSync(process.env.TAW_REPORT_FILE, "utf8"));
const start = (id) => ({ hook_event_name: "SessionStart", session_id: id, source: "startup", taw_agent: "kilo" });

test("a session goes to the report file, a state nowhere, and taw is never run", async () => {
  delete process.env.TMUX_PANE;
  const { TawAgentStatePlugin } = await import(pathToFileURL(process.env.TAW_PLUGIN).href);
  const hooks = await TawAgentStatePlugin();
  await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_kilo_1" } } });
  await hooks.event({ event: { type: "session.idle", properties: { sessionID: "ses_kilo_1" } } });

  assert.ok(existsSync(process.env.TAW_REPORT_FILE));
  assert.deepEqual(JSON.parse(readFileSync(process.env.TAW_REPORT_FILE, "utf8")), {
    hook_event_name: "SessionStart", session_id: "ses_kilo_1", source: "startup", taw_agent: "kilo",
  });
  assert.deepEqual(readdirSync(dirname(process.env.TAW_REPORT_FILE)), ["session.json"]);
  assert.equal(requests.length, 0);
});

test("the report file is taken first: with TMUX_PANE set as well, every event writes only the report and taw is never run", async () => {
  process.env.TMUX_PANE = "%1";
  const hooks = await plugin();
  await hooks["chat.message"]({ sessionID: "ses_kilo_2" });
  await hooks.event({ event: { type: "session.updated", properties: { sessionID: "ses_kilo_2" } } });
  await hooks.event({ event: { type: "session.status", properties: { sessionID: "ses_kilo_2", status: "busy" } } });
  await hooks.event({ event: { type: "permission.asked", properties: { sessionID: "ses_kilo_2" } } });
  await hooks.event({ event: { type: "session.idle", properties: { sessionID: "ses_kilo_2" } } });

  assert.deepEqual(report(), start("ses_kilo_2"));
  assert.deepEqual(readdirSync(dirname(process.env.TAW_REPORT_FILE)), ["session.json"]);
  assert.equal(requests.length, 0);
});

test("a status Kilo names no state for reports the session, as it does outside", async () => {
  const hooks = await plugin();
  await hooks.event({ event: { type: "session.status", properties: { sessionID: "ses_kilo_3", status: "retrying" } } });

  assert.deepEqual(report(), start("ses_kilo_3"));
  assert.equal(requests.length, 0);
});

test("with TAW_REPORT_AGENT naming another agent nothing is written, and its own name writes", async () => {
  process.env.TAW_REPORT_AGENT = "claude";
  const hooks = await plugin();
  await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_kilo_4" } } });
  assert.equal(existsSync(process.env.TAW_REPORT_FILE), false);
  assert.deepEqual(readdirSync(dirname(process.env.TAW_REPORT_FILE)), []);

  process.env.TAW_REPORT_AGENT = "kilo";
  await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_kilo_4" } } });
  assert.deepEqual(report(), start("ses_kilo_4"));
  assert.equal(requests.length, 0);
});

test("a gone report folder ends quietly and leaves nothing behind", async () => {
  process.env.TAW_REPORT_FILE = join(base, "gone", "session.json");
  const hooks = await plugin();
  await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_kilo_5" } } });
  assert.equal(existsSync(join(base, "gone")), false);
  assert.equal(requests.length, 0);
});

const child = (id, parent) => ({ sessionID: id, info: { id, parentID: parent } });

test("inside, a sub-agent's session is never written: its created and status events write nothing, the root's next event writes again", async () => {
  const hooks = await plugin();
  await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_root", info: { id: "ses_root" } } } });
  assert.deepEqual(report(), start("ses_root"));

  await hooks.event({ event: { type: "session.created", properties: child("ses_sub", "ses_root") } });
  await hooks.event({ event: { type: "session.status", properties: { sessionID: "ses_sub", status: "retrying" } } });
  await hooks.event({ event: { type: "session.updated", properties: { sessionID: "ses_sub" } } });
  assert.deepEqual(report(), start("ses_root"));
  assert.deepEqual(readdirSync(dirname(process.env.TAW_REPORT_FILE)), ["session.json"]);

  await hooks.event({ event: { type: "session.status", properties: { sessionID: "ses_root", status: "retrying" } } });
  assert.deepEqual(report(), start("ses_root"));
  assert.equal(requests.length, 0);
});

test("inside, a sub-agent known only from an update, and one under another sub-agent, writes nothing, whichever event comes next", async () => {
  const hooks = await plugin();
  await hooks.event({ event: { type: "session.updated", properties: child("ses_sub_a", "ses_root") } });
  await hooks.event({ event: { type: "session.updated", properties: child("ses_sub_b", "ses_sub_a") } });
  await hooks.event({ event: { type: "session.status", properties: { sessionID: "ses_sub_a", status: "retrying" } } });
  await hooks.event({ event: { type: "session.status", properties: { sessionID: "ses_sub_b", status: "retrying" } } });
  assert.equal(existsSync(process.env.TAW_REPORT_FILE), false);

  await hooks.event({ event: { type: "session.updated", properties: { sessionID: "ses_root", info: { id: "ses_root", title: "t" } } } });
  assert.deepEqual(report(), start("ses_root"));
});

test("outside a sandbox a sub-agent's session is told to taw as today", async () => {
  delete process.env.TAW_REPORT_FILE;
  process.env.TMUX_PANE = "%1";
  const hooks = await plugin();
  await hooks.event({ event: { type: "session.created", properties: child("ses_sub_c", "ses_root") } });
  await hooks.event({ event: { type: "session.status", properties: { sessionID: "ses_sub_c", status: "retrying" } } });
  assert.deepEqual(requests.map((r) => [r.method, r.params.agent_session_id]), [
    ["pane.report_agent_session", "ses_sub_c"],
    ["pane.report_agent_session", "ses_sub_c"],
  ]);
});

test("outside a sandbox it runs as today: with TMUX_PANE alone, taw is told the session", async () => {
  delete process.env.TAW_REPORT_FILE;
  process.env.TMUX_PANE = "%1";
  const hooks = await plugin();
  await hooks.event({ event: { type: "session.created", properties: { sessionID: "ses_kilo_6" } } });
  assert.equal(requests.length, 1);
  assert.equal(requests[0].method, "pane.report_agent_session");
  assert.equal(requests[0].params.agent_session_id, "ses_kilo_6");
});

test("with neither TMUX_PANE nor TAW_REPORT_FILE it hooks nothing", async () => {
  delete process.env.TAW_REPORT_FILE;
  assert.deepEqual(await plugin(), {});
});
