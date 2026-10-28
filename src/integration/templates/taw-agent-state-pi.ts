// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// taw applies writes in arrival order, so a coalesced, strictly serialised report queue is what
// stops a late report overwriting a newer state. Inside a sandbox (TAW_REPORT_FILE set) it writes
// the session to that file instead, and writes nothing when TAW_REPORT_AGENT names another agent.
//
// Pi's extension events cover the whole lifecycle, so this extension is the pane's state
// authority and reports idle/working/blocked as well as session identity.
//
// installed by: taw integration install {{AGENT}}
// managed by taw; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
//
// TAW_INTEGRATION_AGENT={{AGENT}}
// TAW_INTEGRATION_VERSION=2
// @ts-nocheck

import { execFile } from "node:child_process";
import { renameSync, rmSync, writeFileSync } from "node:fs";

const TAW_BIN = {{TAW_BIN}};
const AGENT = "{{AGENT}}";
const SOURCE = `integration:${AGENT}`;

function paneId(): string | undefined {
  return process.env.TMUX_PANE || undefined;
}

function enabled(): boolean {
  return !!paneId() || !!process.env.TAW_REPORT_FILE;
}

// One taw invocation, resolving when it finishes (or fails). Never rejects: a reporting
// failure must not break the user's session.
function runTaw(args: string[]): Promise<void> {
  const pane = paneId();
  if (process.env.TAW_REPORT_FILE || !pane) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    try {
      execFile(TAW_BIN, args, { timeout: 5000 }, () => resolve());
    } catch {
      resolve();
    }
  });
}

// Inside a sandbox (taw's own): the session goes to TAW_REPORT_FILE in the shape
// of Claude's SessionStart, beside the file and renamed into place, and nothing else ever does:
// no state, and no taw is run. taw reads it from outside.
function writeReport(report: Record<string, string | undefined>): void {
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

// Strictly serialised: taw applies writes in the order they arrive, so two reports in
// flight at once could land newest-first and leave the pane showing the older state.
let requestQueue: Promise<void> = Promise.resolve();

function sendRequest(args: string[]): Promise<void> {
  requestQueue = requestQueue.then(
    () => runTaw(args),
    () => runTaw(args),
  );
  return requestQueue;
}

type AgentState = "working" | "blocked" | "idle";

type QueuedState = {
  state: AgentState;
  message?: string;
  seq: number;
};

let reportSeq = Date.now() * 1000;
let currentAgentSessionId: string | undefined;
let currentAgentSessionPath: string | undefined;

function nextReportSeq(): number {
  reportSeq += 1;
  return reportSeq;
}

function updateSessionRef(ctx: any): void {
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    currentAgentSessionPath =
      typeof file === "string" && file.startsWith("/") ? file : undefined;
  } catch {
    currentAgentSessionPath = undefined;
  }

  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    currentAgentSessionId = typeof id === "string" && id.length > 0 ? id : undefined;
  } catch {
    currentAgentSessionId = undefined;
  }
}

// The path is preferred over the id: pi resumes from either, and a path survives a
// session the id no longer resolves.
function sessionRefArgs(): string[] | undefined {
  if (currentAgentSessionPath) {
    return ["--agent-session-path", currentAgentSessionPath];
  }
  if (currentAgentSessionId) {
    return ["--agent-session-id", currentAgentSessionId];
  }
  return undefined;
}

function reportSession(sessionStartSource?: string): Promise<void> {
  if (process.env.TAW_REPORT_FILE) {
    // What the hook would report: the session file's path where there is one, else the id.
    const ref = currentAgentSessionPath
      ? { transcript_path: currentAgentSessionPath }
      : currentAgentSessionId ? { session_id: currentAgentSessionId } : undefined;
    if (ref) writeReport({ ...ref, source: sessionStartSource });
    return Promise.resolve();
  }
  const pane = paneId();
  const ref = sessionRefArgs();
  if (!pane || !ref) {
    return Promise.resolve();
  }

  const args = [
    "pane", "report-agent-session", pane,
    "--source", SOURCE,
    "--agent", AGENT,
    "--seq", String(nextReportSeq()),
    ...ref,
  ];
  if (sessionStartSource) {
    args.push("--session-start-source", sessionStartSource);
  }
  return sendRequest(args);
}

function sendState(state: AgentState, message?: string, seq = nextReportSeq()): Promise<void> {
  const pane = paneId();
  if (!pane) {
    return Promise.resolve();
  }
  const args = [
    "pane", "report-agent", pane,
    "--source", SOURCE,
    "--agent", AGENT,
    "--seq", String(seq),
    "--state", state,
  ];
  if (message) {
    args.push("--message", message);
  }
  const ref = sessionRefArgs();
  if (ref) {
    args.push(...ref);
  }
  return sendRequest(args);
}

let sendInFlight = false;
let queuedState: QueuedState | undefined;

// Coalescing: only the most recent state is worth sending, so a burst of transitions
// while one report is in flight collapses to its last value.
function queueState(state: AgentState, message?: string): void {
  queuedState = { state, message, seq: nextReportSeq() };
  if (!sendInFlight) {
    void drainStateQueue();
  }
}

async function drainStateQueue(): Promise<void> {
  if (sendInFlight) {
    return;
  }

  sendInFlight = true;
  try {
    while (queuedState) {
      const next = queuedState;
      queuedState = undefined;
      await sendState(next.state, next.message, next.seq);
    }
  } finally {
    sendInFlight = false;
    if (queuedState) {
      void drainStateQueue();
    }
  }
}

export default function (pi) {
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let blockedCount = 0;
  let blockedMessage: string | undefined;
  let lastState: AgentState | undefined;
  let lastMessage: string | undefined;
  let rootSession = false;

  function desiredState() {
    if (blockedCount > 0) {
      return { state: "blocked" as const, message: blockedMessage };
    }
    if (agentActive) {
      return { state: "working" as const, message: undefined };
    }
    return { state: "idle" as const, message: undefined };
  }

  function publishState(force = false) {
    const next = desiredState();
    if (!force && next.state === lastState && next.message === lastMessage) {
      return;
    }
    lastState = next.state;
    lastMessage = next.message;
    queueState(next.state, next.message);
  }

  pi.events.on("taw:blocked", (data) => {
    if (!rootSession) {
      return;
    }
    if (!data?.active) {
      blockedCount = Math.max(0, blockedCount - 1);
      if (blockedCount === 0) {
        blockedMessage = undefined;
      }
      publishState();
      return;
    }

    blockedCount += 1;
    blockedMessage = data.label;
    publishState();
  });

  pi.on("session_start", async (event, ctx) => {
    // TUI only: RPC/JSON/print modes are headless (no pane taw can display), and RPC
    // still reports hasUI=true, so mode is the reliable gate.
    if (ctx?.mode !== "tui") {
      return;
    }
    rootSession = true;
    updateSessionRef(ctx);
    await reportSession(event?.reason);
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession) {
      return;
    }
    updateSessionRef(ctx);
    void reportSession();
    agentActive = true;
    publishState();
  });

  pi.on("agent_settled", (_event, ctx) => {
    if (!rootSession || ctx?.isIdle?.() !== true) {
      return;
    }

    agentActive = false;
    publishState();
  });
}
