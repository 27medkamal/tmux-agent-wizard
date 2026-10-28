// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// taw applies writes in arrival order, so a coalesced, strictly serialised report queue is what
// stops a late report overwriting a newer state. An `agent_end` event that is not a real settle
// (`willContinue`) is ignored. OMP marks every shell it spawns with OMPCODE=1, so a nested `omp`
// launched from a parent session's shell inherits it; `enabled()` refuses that nested process,
// which is not the pane's root agent. The timing knobs are $TAW_OMP_IDLE_DEBOUNCE_MS and
// $TAW_OMP_RETRY_GRACE_MS. Inside a sandbox (TAW_REPORT_FILE set) it writes the session to that
// file instead, and writes nothing when TAW_REPORT_AGENT names another agent.
//
// OMP's extension events cover the whole lifecycle, so this extension is the pane's state
// authority and reports idle/working/blocked as well as session identity. It also holds
// the pane in Working through OMP's own auto-retry of a provider error, rather than
// flickering to Idle and back.
//
// installed by: taw integration install {{AGENT}}
// managed by taw; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
//
// TAW_INTEGRATION_AGENT={{AGENT}}
// TAW_INTEGRATION_VERSION=4
// @ts-nocheck

import { execFile } from "node:child_process";
import { renameSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";

const TAW_BIN = {{TAW_BIN}};
const AGENT = "{{AGENT}}";
const SOURCE = `integration:${AGENT}`;

function paneId(): string | undefined {
  return process.env.TMUX_PANE || undefined;
}

function enabled(): boolean {
  return (!!paneId() || !!process.env.TAW_REPORT_FILE) && process.env.OMPCODE !== "1";
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

function parseDurationEnv(name: string, fallback: number): number {
  const raw = process.env[name];
  if (!raw) {
    return fallback;
  }
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return fallback;
  }
  return parsed;
}

const idleDebounceMs = parseDurationEnv("TAW_OMP_IDLE_DEBOUNCE_MS", 250);
const retryGraceMs = parseDurationEnv("TAW_OMP_RETRY_GRACE_MS", 2500);
// A provider error OMP will retry by itself. Matched so a retry reads as Working rather
// than a flicker to Idle followed by Blocked.
const retryableErrorPattern =
  /overloaded|provider.?returned.?error|rate.?limit|too many requests|429|500|502|503|504|service.?unavailable|server.?error|internal.?error|network.?error|connection.?error|connection.?refused|connection.?lost|websocket.?closed|websocket.?error|other side closed|fetch failed|upstream.?connect|reset before headers|socket hang up|ended without|http2 request did not get a response|timed? out|timeout|terminated|retry delay/i;
let reportSeq = Date.now() * 1000;
let currentAgentSessionId: string | undefined;
let currentAgentSessionPath: string | undefined;

function nextReportSeq(): number {
  reportSeq += 1;
  return reportSeq;
}

export function isAbsoluteSessionPath(file: unknown): file is string {
  return (
    typeof file === "string" &&
    (path.posix.isAbsolute(file) || path.win32.isAbsolute(file))
  );
}

function updateSessionRef(ctx: any): void {
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    currentAgentSessionPath = isAbsoluteSessionPath(file) ? file : undefined;
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

function reportSession(sessionStartSource = "startup"): Promise<void> {
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

function lastAssistantMessage(messages: unknown[]): any | undefined {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const message = messages[i] as any;
    if (message?.role === "assistant") {
      return message;
    }
  }
  return undefined;
}

function retryableErrorMessage(event: any): string | undefined {
  const messages = Array.isArray(event?.messages) ? event.messages : [];
  const assistant = lastAssistantMessage(messages);
  if (assistant?.stopReason !== "error") {
    return undefined;
  }

  const errorMessage = String(assistant.errorMessage ?? "");
  if (!retryableErrorPattern.test(errorMessage)) {
    return undefined;
  }
  return errorMessage || "retryable provider error";
}

function askBlockedMessage(args: any): string {
  const questions = Array.isArray(args?.questions) ? args.questions : [];
  const firstQuestion = questions.find((question: any) => typeof question?.question === "string");
  if (firstQuestion?.question) {
    return firstQuestion.question;
  }
  return "waiting for user input";
}

export default function (pi) {
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let retryHoldActive = false;
  let failureBlocked = false;
  let failureMessage: string | undefined;
  let blockedCount = 0;
  let blockedMessage: string | undefined;
  let lastState: AgentState | undefined;
  let lastMessage: string | undefined;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let rootSession = false;

  function clearTimer(timer: ReturnType<typeof setTimeout> | undefined) {
    if (timer) {
      clearTimeout(timer);
    }
  }

  function clearPendingTimers() {
    clearTimer(idleTimer);
    clearTimer(retryTimer);
    idleTimer = undefined;
    retryTimer = undefined;
  }

  function clearFailureState() {
    retryHoldActive = false;
    failureBlocked = false;
    failureMessage = undefined;
  }

  function desiredState() {
    if (blockedCount > 0) {
      return { state: "blocked" as const, message: blockedMessage };
    }
    if (failureBlocked) {
      return { state: "blocked" as const, message: failureMessage };
    }
    if (agentActive || retryHoldActive) {
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

  function scheduleIdle() {
    clearPendingTimers();
    clearFailureState();
    idleTimer = setTimeout(() => {
      idleTimer = undefined;
      publishState();
    }, idleDebounceMs);
    idleTimer.unref?.();
  }

  function holdForRetry(message: string) {
    clearPendingTimers();
    retryHoldActive = true;
    failureBlocked = false;
    failureMessage = message;
    publishState();

    retryTimer = setTimeout(() => {
      retryTimer = undefined;
      retryHoldActive = false;
      failureBlocked = true;
      publishState();
    }, retryGraceMs);
    retryTimer.unref?.();
  }

  function activateRootSession(ctx: any, sessionStartSource = "startup"): boolean {
    if (ctx?.hasUI !== true) {
      return false;
    }
    rootSession = true;
    updateSessionRef(ctx);
    void reportSession(sessionStartSource);
    return true;
  }

  function resetSessionState() {
    clearPendingTimers();
    clearFailureState();
    agentActive = false;
    blockedCount = 0;
    blockedMessage = undefined;
  }

  function activateBlocked(message: string | undefined) {
    clearPendingTimers();
    blockedCount += 1;
    blockedMessage = message;
    publishState();
  }

  function deactivateBlocked() {
    blockedCount = Math.max(0, blockedCount - 1);
    if (blockedCount === 0) {
      blockedMessage = undefined;
    }
    publishState();
  }

  pi.events.on("taw:blocked", (data) => {
    if (!rootSession) {
      return;
    }
    if (!data?.active) {
      deactivateBlocked();
      return;
    }

    activateBlocked(data.label);
  });

  pi.on("session_start", (_event, ctx) => {
    if (!activateRootSession(ctx)) {
      return;
    }
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("session_switch", (event, ctx) => {
    if (!activateRootSession(ctx, event?.reason || "resume")) {
      return;
    }
    resetSessionState();
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    updateSessionRef(ctx);
    void reportSession();
    clearPendingTimers();
    clearFailureState();
    agentActive = true;
    publishState();
  });

  pi.on("tool_approval_requested", (event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    const label = event?.reason || `${event?.toolName || "Tool"} approval`;
    activateBlocked(label);
  });

  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
  });

  pi.on("tool_execution_start", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    activateBlocked(askBlockedMessage(event.args));
  });

  pi.on("tool_execution_end", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
  });

  pi.on("agent_end", (event) => {
    if (!rootSession) {
      return;
    }
    if (!agentActive) {
      // OMP can emit duplicate/late end events while auto-retry is already holding the
      // pane in Working. Do not let an unqualified duplicate end cancel the retry hold
      // and publish a false Idle.
      return;
    }
    if (event?.willContinue === true) {
      // A continuation is already scheduled, so this end is not a settle.
      // Older builds omit the field and fall through as before.
      return;
    }

    agentActive = false;

    const retryableMessage = retryableErrorMessage(event);
    if (retryableMessage) {
      holdForRetry(retryableMessage);
      return;
    }

    scheduleIdle();
  });

  pi.on("session_shutdown", () => {
    if (rootSession) {
      clearPendingTimers();
    }
  });
}
