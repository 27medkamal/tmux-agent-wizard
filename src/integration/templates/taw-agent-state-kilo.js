// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// Inside a sandbox (TAW_REPORT_FILE set) it writes the session to that file instead, and writes
// nothing when TAW_REPORT_AGENT names another agent, or for a sub-agent's session.
//
// Kilo's plugin events cover the whole lifecycle, so this plugin is the pane's state
// authority and reports idle/working/blocked as well as session identity.
//
// installed by: taw integration install {{AGENT}}
// managed by taw; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
//
// TAW_INTEGRATION_AGENT={{AGENT}}
// TAW_INTEGRATION_VERSION=2

import { execFile } from "node:child_process";
import { renameSync, rmSync, writeFileSync } from "node:fs";

const TAW_BIN = {{TAW_BIN}};
const AGENT = "{{AGENT}}";

// herdr's plugin keeps a monotonic counter so out-of-order reports can be discarded;
// `--seq` keeps the same guarantee here.
let reportSeq = Date.now() * 1000;

function nextReportSeq() {
  reportSeq += 1;
  return reportSeq;
}

// Fire-and-forget: never await, never throw, never let a taw invocation affect the
// agent's own turn.
function runTaw(args) {
  if (process.env.TAW_REPORT_FILE || !process.env.TMUX_PANE) return;
  try {
    execFile(TAW_BIN, args, { timeout: 5000 }, () => {});
  } catch {
    // ignore: a reporting failure must never break the user's session
  }
}

// Inside a sandbox (taw's own): the session goes to TAW_REPORT_FILE in the shape
// of Claude's SessionStart, beside the file and renamed into place, and nothing else ever does:
// no state, and no taw is run. taw reads it from outside.
function writeReport(report) {
  // Another agent's plugin in here writes nothing over the sandbox's own agent's report.
  const named = process.env.TAW_REPORT_AGENT;
  if (named && named !== AGENT) return;
  const file = process.env.TAW_REPORT_FILE;
  const staged = `${file}.${process.pid}`;
  try {
    writeFileSync(staged, JSON.stringify({ hook_event_name: "SessionStart", ...report, taw_agent: AGENT }));
    renameSync(staged, file);
  } catch {
    try { rmSync(staged, { force: true }); } catch {}
  }
}

// Sessions created or updated with a parent are a sub-agent's. Inside a sandbox none of them
// is written to the report file, so the pane's own session is the one taw reads.
const childSessions = new Set();

function sessionIDFromProperties(properties) {
  return typeof properties?.sessionID === "string" && properties.sessionID
    ? properties.sessionID
    : undefined;
}

function stateFromSessionStatus(status) {
  if (typeof status !== "string") {
    return undefined;
  }
  switch (status.toLowerCase()) {
    case "idle":
      return "idle";
    case "active":
    case "busy":
    case "pending":
    case "running":
    case "streaming":
    case "working":
      return "working";
    default:
      return undefined;
  }
}

function reportSession(sessionID) {
  if (!sessionID) return;
  if (process.env.TAW_REPORT_FILE) {
    if (childSessions.has(sessionID)) return;
    writeReport({ session_id: sessionID, source: "startup" });
    return;
  }
  runTaw([
    "pane", "report-agent-session", process.env.TMUX_PANE,
    "--source", `integration:${AGENT}`,
    "--agent", AGENT,
    "--seq", String(nextReportSeq()),
    "--agent-session-id", sessionID,
    "--session-start-source", "startup",
  ]);
}

function reportState(state, sessionID) {
  const args = [
    "pane", "report-agent", process.env.TMUX_PANE,
    "--source", `integration:${AGENT}`,
    "--agent", AGENT,
    "--seq", String(nextReportSeq()),
    "--state", state,
  ];
  if (sessionID) args.push("--agent-session-id", sessionID);
  runTaw(args);
}

export const TawAgentStatePlugin = async () => {
  if (!process.env.TMUX_PANE && !process.env.TAW_REPORT_FILE) {
    return {};
  }

  return {
    "chat.message": async ({ sessionID }) => {
      reportState("working", sessionID);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      const info = properties.info;
      if (info?.id && info.parentID) childSessions.add(info.id);

      switch (type) {
        case "session.created":
        case "session.updated":
          reportSession(sessionID);
          break;
        case "session.status": {
          const state = stateFromSessionStatus(properties.status);
          if (state) {
            reportState(state, sessionID);
          } else {
            reportSession(sessionID);
          }
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          reportState("working", sessionID);
          break;
        case "permission.asked":
        case "question.asked":
        case "session.error":
          reportState("blocked", sessionID);
          break;
        case "session.idle":
          reportState("idle", sessionID);
          break;
        case "session.deleted":
          break;
        default:
          break;
      }
    },
  };
};
