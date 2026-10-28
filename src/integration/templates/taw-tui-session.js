// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// A stale report is dropped as it leaves the queue, before taw is run, since nothing here has a
// connection to wait for. Inside a sandbox (`TAW_REPORT_FILE` set) a selection is written to that
// file instead, and nothing is written when `TAW_REPORT_AGENT` names another agent.
//
// This plugin runs in the TUI of the pane it reports for. OpenCode 2's one shared server
// cannot tell which pane a session belongs to; the TUI can, so it is the TUI that reports.
//
// installed by: taw integration install {{AGENT}}
// managed by taw; reinstalling or updating the integration overwrites this file.
//
// TAW_INTEGRATION_AGENT={{AGENT}}
// TAW_INTEGRATION_VERSION=5

import { execFile } from "node:child_process";
import { renameSync, rmSync, writeFileSync } from "node:fs";

const TAW_BIN = {{TAW_BIN}};
const AGENT = "{{AGENT}}";
const SOURCE = `integration:${AGENT}`;
const ROUTE_POLL_INTERVAL_MS = 100;
const SELECTION_RETRY_DELAYS_MS = [100, 400, 1_000];
// How long one run of taw may take before it counts as undelivered and the report is retried.
const REPORT_TIMEOUT_MS = 5_000;

// Inside a sandbox (taw's own): the profile names a file in TAW_REPORT_FILE, in
// the pane's report folder. A selection is written there in the shape of Claude's SessionStart,
// beside the file and renamed into place, and nothing else ever is: no state, and no taw is run.
// taw reads it from outside.
function writeReport(file, sessionID) {
  const staged = `${file}.${process.pid}`;
  try {
    writeFileSync(staged, JSON.stringify({
      hook_event_name: "SessionStart",
      session_id: sessionID,
      source: "select",
      taw_agent: AGENT,
    }));
    renameSync(staged, file);
  } catch {
    try { rmSync(staged, { force: true }); } catch {}
  }
}

function requestOnce(sessionID, state, seq, isCurrent = () => true) {
  const report = process.env.TAW_REPORT_FILE;
  if (report) {
    // Another agent's plugin in here writes nothing over the sandbox's own agent's report.
    const named = process.env.TAW_REPORT_AGENT;
    if (named && named !== AGENT) return Promise.resolve(true);
    // A report that could not be written is not retried: nothing outside waits for it.
    if (state === undefined && isCurrent()) writeReport(report, sessionID);
    return Promise.resolve(true);
  }
  const pane = process.env.TMUX_PANE;
  if (!pane) {
    return Promise.resolve(true);
  }

  const args = state === undefined
    ? [
        "pane", "report-agent-session", pane,
        "--source", SOURCE,
        "--agent", AGENT,
        "--agent-session-id", sessionID,
        "--session-start-source", "select",
      ]
    : [
        "pane", "report-agent", pane,
        "--source", SOURCE,
        "--agent", AGENT,
        "--seq", String(seq),
        "--state", state,
        "--agent-session-id", sessionID,
      ];

  return new Promise((resolve) => {
    if (!isCurrent()) {
      resolve(false);
      return;
    }
    try {
      // execFile calls back once, when taw has exited or been killed at the timeout.
      execFile(TAW_BIN, args, { timeout: REPORT_TIMEOUT_MS }, (error) => resolve(!error));
    } catch {
      resolve(false);
    }
  });
}

export default {
  id: "taw.opencode.session-selection",
  // Keep this plain object dependency-free: V1 and V2 expose different SDK
  // packages, but both loaders accept their own lifecycle entry on this object.
  setup,
  tui,
};

async function tui(api) {
  if (!process.env.TMUX_PANE && !process.env.TAW_REPORT_FILE) return;

  let disposed = false;
  let context;
  let sequence = Date.now() * 1000;
  let chain = Promise.resolve();

  function routeID() {
    const route = api.route.current;
    return route?.name === "session" ? route.params?.sessionID : undefined;
  }

  function current(ctx) {
    return !disposed && context === ctx && routeID() === ctx.route;
  }

  async function read(ctx, request) {
    const result = await request({
      signal: AbortSignal.any([ctx.controller.signal, AbortSignal.timeout(5_000)]),
      throwOnError: true,
    });
    if (!current(ctx) || result?.data === undefined) throw new Error("session data unavailable");
    return result.data;
  }

  // The first selected session owns this pane's subtree. Browsing descendants
  // keeps that boundary; directly attaching to a child does not claim siblings.
  function root(ctx, id) {
    const seen = new Set();
    while (typeof id === "string" && !seen.has(id) && !ctx.deleted.has(id)) {
      seen.add(id);
      if (id === ctx.boundary) return id;
      const session = ctx.sessions.get(id) ?? api.state.session.get(id);
      if (!session) return;
      if (!session.parentID) return id;
      id = session.parentID;
    }
  }

  async function resolveRoot(ctx, id) {
    const seen = new Set();
    while (typeof id === "string" && !seen.has(id) && !ctx.deleted.has(id)) {
      seen.add(id);
      if (id === ctx.boundary) return id;
      let session = ctx.sessions.get(id) ?? api.state.session.get(id);
      if (!session) {
        if (!ctx.lookups.has(id)) {
          const requestedID = id;
          const lookup = read(ctx, (options) => api.client.session.get({ sessionID: requestedID }, options));
          ctx.lookups.set(id, lookup);
          lookup.finally(() => ctx.lookups.delete(requestedID)).catch(() => {});
        }
        session = await ctx.lookups.get(id);
        if (ctx.deleted.has(id)) return;
        session = ctx.sessions.get(id) ?? session;
        if (session.id !== id) throw new Error("unexpected session identity");
        ctx.sessions.set(id, session);
      }
      if (!session.parentID) return id;
      id = session.parentID;
    }
  }

  function owners(ctx) {
    return new Set([...ctx.statuses.keys(), ...[...ctx.blockers.values()].map((r) => r.sessionID)]);
  }

  function state(ctx) {
    if (ctx.settled) return "idle";
    for (const request of ctx.blockers.values()) {
      if (root(ctx, request.sessionID) === ctx.selected) return "blocked";
    }
    if (ctx.errors.has(ctx.selected)) return "blocked";
    for (const id of ctx.statuses.keys()) {
      if (root(ctx, id) === ctx.selected) return "working";
    }
    if (ctx.hydrated && [...owners(ctx)].every((id) => root(ctx, id) !== undefined)) return "idle";
  }

  function publish(ctx, selection = false) {
    if (!current(ctx) || !ctx.selected) return;
    ctx.selectionPending ||= selection;
    if (ctx.queued) return;
    ctx.queued = true;
    chain = chain.then(async () => {
      ctx.queued = false;
      const selected = ctx.selected;
      const isCurrent = () => current(ctx) && !!selected && ctx.selected === selected &&
        (ctx.settled || root(ctx, ctx.route) === selected);
      if (!isCurrent()) return;
      if (ctx.selectionPending) {
        if (!await requestOnce(selected, undefined, undefined, isCurrent)) {
          ctx.retryAt = Date.now() + 500;
          return;
        }
        if (!isCurrent()) return;
        ctx.selectionPending = false;
        ctx.lastState = undefined;
      }
      const value = state(ctx);
      if (value === undefined || value === ctx.lastState) return;
      const delivered = await requestOnce(selected, value, ++sequence, isCurrent);
      if (!isCurrent()) return;
      ctx.lastState = delivered ? value : undefined;
      if (!delivered) ctx.retryAt = Date.now() + 500;
    }).catch(() => {
      if (current(ctx)) {
        ctx.lastState = undefined;
        ctx.retryAt = Date.now() + 500;
      }
    });
  }

  function retire(ctx, route) {
    ctx?.controller?.abort();
    context = ctx?.selected
      ? { route, selected: ctx.selected, settled: true, retryAt: Infinity }
      : undefined;
    if (context) publish(context);
  }

  function reconcile(ctx) {
    if (!current(ctx)) return;
    publish(ctx);
    if (ctx.resolving) {
      ctx.resolveAgain = true;
      return;
    }
    ctx.resolving = true;
    void (async () => {
      const ancestor = await resolveRoot(ctx, ctx.route);
      if (!current(ctx) || !ancestor) return;
      const selected = ancestor === ctx.boundary ? ctx.boundary : ctx.route;
      if (selected !== ctx.selected) {
        ctx.boundary = selected;
        ctx.selected = selected;
        publish(ctx, true);
      }
      // Fetch ancestry only for active/request-bearing sessions, not historical trees.
      await Promise.all([...owners(ctx)].map((id) => resolveRoot(ctx, id)));
      if (current(ctx)) publish(ctx);
    })().catch(() => {
      if (current(ctx)) ctx.retryAt = Date.now() + 500;
    }).finally(() => {
      ctx.resolving = false;
      if (ctx.resolveAgain) {
        ctx.resolveAgain = false;
        reconcile(ctx);
      }
    });
  }

  function clearRequests(ctx, matches) {
    for (const [key, request] of ctx.blockers) if (matches(request)) ctx.blockers.delete(key);
  }

  function terminalTool(part) {
    return part?.type === "tool" && ["completed", "error"].includes(part.state?.status);
  }

  function apply(ctx, event) {
    const data = event.properties;
    if (!data) return;
    if (event.type === "message.part.updated") {
      const part = data.part;
      if (terminalTool(part)) {
        clearRequests(ctx, (r) => r.sessionID === part.sessionID &&
          r.tool?.messageID === part.messageID && r.tool?.callID === part.callID);
      }
      return;
    }
    const id = data.sessionID ?? data.info?.id;
    if (typeof id !== "string" || ctx.deleted.has(id)) return;
    switch (event.type) {
      case "session.created":
      case "session.updated":
        if (data.info) ctx.sessions.set(id, data.info);
        break;
      case "session.deleted":
        ctx.deleted.add(id);
        ctx.sessions.delete(id);
        ctx.statuses.delete(id);
        ctx.errors.delete(id);
        clearRequests(ctx, (r) => r.sessionID === id);
        if (ctx.selected === id) retire(ctx, ctx.route);
        break;
      case "session.status":
      case "session.idle": {
        const status = event.type === "session.idle" ? "idle" : data.status?.type;
        if (status === "busy" || status === "retry") {
          ctx.statuses.set(id, status);
          ctx.errors.delete(id);
        } else if (status === "idle") {
          ctx.statuses.delete(id);
          ctx.errors.delete(id);
        }
        break;
      }
      case "session.error":
        if (data.error?.name !== "MessageAbortedError") ctx.errors.add(id);
        break;
      case "permission.asked":
      case "question.asked":
        if (typeof data.id === "string") ctx.blockers.set(`${event.type.split(".")[0]}:${data.id}`, data);
        break;
      case "permission.replied":
      case "question.replied":
      case "question.rejected":
        ctx.blockers.delete(`${event.type.split(".")[0]}:${data.requestID}`);
        break;
    }
  }

  async function hydrate(ctx) {
    if (!current(ctx) || ctx.loading) return;
    ctx.loading = true;
    try {
      const [statuses, permissions, questions] = await Promise.all([
        read(ctx, (options) => api.client.session.status(undefined, options)),
        read(ctx, (options) => api.client.permission.list(undefined, options)),
        read(ctx, (options) => api.client.question.list(undefined, options)),
      ]);
      if (!statuses || typeof statuses !== "object" || Array.isArray(statuses) ||
          !Object.values(statuses).every((s) => ["busy", "retry", "idle"].includes(s?.type)) ||
          !Array.isArray(permissions) || !Array.isArray(questions)) {
        throw new Error("incomplete session snapshot");
      }
      const messages = new Map();
      const blockers = new Map();
      let validated = true;
      await Promise.all([["permission", permissions], ["question", questions]].flatMap(([kind, requests]) =>
        requests.map(async (request) => {
          if (ctx.deleted.has(request.sessionID)) return;
          if (request.tool) {
            const { messageID, callID } = request.tool;
            const key = `${request.sessionID}:${messageID}`;
            if (!messages.has(key)) messages.set(key, read(ctx, (options) => api.client.session.message({
              sessionID: request.sessionID, messageID,
            }, options)));
            try {
              const message = await messages.get(key);
              const part = message.parts?.find((p) => p.type === "tool" && p.callID === callID);
              // OpenCode can retain pending requests after abort. Status alone is
              // insufficient: the same session may already be running a new turn.
              if (terminalTool(part)) return;
            } catch {
              validated = false;
            }
          }
          if (!ctx.deleted.has(request.sessionID)) blockers.set(`${kind}:${request.id}`, request);
        })));
      if (!current(ctx)) return;
      ctx.statuses = new Map(Object.entries(statuses)
        .filter(([id, status]) => !ctx.deleted.has(id) && (status.type === "busy" || status.type === "retry")));
      ctx.blockers = blockers;
      // Preserve deltas until hydration succeeds, including retry backoff:
      // a later snapshot must not revive a replied or cancelled request.
      for (const event of ctx.events) apply(ctx, event);
      ctx.hydrated = validated;
      if (!validated) ctx.retryAt = Date.now() + 500;
      reconcile(ctx);
    } catch {
      if (current(ctx)) ctx.retryAt = Date.now() + 500;
    } finally {
      ctx.loading = false;
      if (ctx.hydrated) ctx.events = [];
    }
  }

  function syncSelection(reset = false) {
    if (disposed) return;
    const id = routeID();
    if (reset || id !== context?.route) {
      if (typeof id !== "string" || !id) {
        retire(context, id);
        return;
      }
      const boundary = context?.settled ? undefined : context?.selected;
      context?.controller?.abort();
      const ctx = {
        route: id, boundary, controller: new AbortController(), sessions: new Map(), lookups: new Map(),
        statuses: new Map(), blockers: new Map(), errors: new Set(), deleted: new Set(),
        events: [], hydrated: false, loading: false, resolving: false,
        retryAt: Infinity, selectionAt: 0, retryIndex: 0,
      };
      context = ctx;
      reconcile(ctx);
      void hydrate(ctx);
    }
    const ctx = context;
    if (!ctx) return;
    if (!ctx.settled && ctx.selected && Date.now() >= ctx.selectionAt) {
      publish(ctx, true);
      const delay = SELECTION_RETRY_DELAYS_MS[ctx.retryIndex++];
      ctx.selectionAt = delay === undefined ? Infinity : Date.now() + delay;
    }
    if (Date.now() >= ctx.retryAt) {
      ctx.retryAt = Infinity;
      if (ctx.settled) publish(ctx);
      else {
        if (!ctx.hydrated) void hydrate(ctx);
        reconcile(ctx);
      }
    }
  }

  const subscriptions = [
    "session.created", "session.updated", "session.deleted", "session.status", "session.idle", "session.error",
    "permission.asked", "permission.replied", "question.asked", "question.replied", "question.rejected",
    "message.part.updated",
  ].map((type) => api.event.on(type, (event) => {
    if (type === "message.part.updated" && !terminalTool(event.properties?.part)) return;
    syncSelection();
    const ctx = context;
    if (!ctx || ctx.settled) return;
    if (ctx.loading || !ctx.hydrated) ctx.events.push(event);
    apply(ctx, event);
    reconcile(ctx);
  }));
  for (const type of ["server.connected", "server.instance.disposed", "global.disposed"]) {
    subscriptions.push(api.event.on(type, () => syncSelection(true)));
  }
  const poll = setInterval(syncSelection, ROUTE_POLL_INTERVAL_MS);
  api.lifecycle.onDispose(() => {
    disposed = true;
    context?.controller?.abort();
    clearInterval(poll);
    for (const unsubscribe of subscriptions) unsubscribe();
  });
  syncSelection();
}

function setup(api) {
  if (!process.env.TMUX_PANE && !process.env.TAW_REPORT_FILE) return;

  let disposed = false;
  let selected;
  let generation = 0;
  let sequence = Date.now() * 1000;
  let chain = Promise.resolve();
  let retryIndex = 0;
  let nextSelectionAt = 0;
  let state = "idle";
  let retryTimer;
  const sessions = new Map();
  let blockers = new Map();
  // Event callbacks may precede cache updates. Retain each delta until the
  // cache reflects it, so late hydration cannot undo a reply or lose an ask.
  const blockerChanges = new Map();

  function root(id) {
    const seen = new Set();
    while (typeof id === "string" && !seen.has(id)) {
      seen.add(id);
      const session = api.data.session.get(id) ?? sessions.get(id);
      if (!session) return;
      if (!session.parentID) return id;
      id = session.parentID;
    }
  }

  function current() {
    const route = api.ui.router.current();
    return route.type === "session" ? root(route.sessionID) : undefined;
  }

  // Selection and lifecycle use one queue. Recheck attribution at dispatch,
  // not just when receiving the event, and reject A -> B -> A stale work too.
  function enqueue(value) {
    const sessionID = selected;
    const revision = generation;
    const isCurrent = () => !disposed && revision === generation && !!sessionID && current() === sessionID;
    chain = chain.then(async () => {
      if (!isCurrent()) return;
      const delivered = await requestOnce(sessionID, value, value === undefined ? undefined : ++sequence, isCurrent);
      if (!delivered) scheduleStateRetry();
    }).catch(() => {});
  }

  // A dropped report must not strand the pane on a stale state once the
  // selection retry schedule has run out: resend the latest state until the
  // socket accepts it or the selection is no longer current.
  function scheduleStateRetry() {
    if (disposed || retryTimer) return;
    retryTimer = setTimeout(() => {
      retryTimer = undefined;
      publish();
    }, 500);
    retryTimer.unref?.();
  }

  function publish() {
    enqueue(blockers.size ? "blocked" : state);
  }

  function changeBlocker(id, kind, requestID, present) {
    if (typeof requestID !== "string") return;
    const key = `${kind}:${requestID}`;
    blockerChanges.set(key, { id, kind, present });
    if (present) blockers.set(key, id);
    else blockers.delete(key);
  }

  function reconcileBlockers() {
    const next = new Map();
    const hydrated = new Set();
    const members = new Set([selected, ...api.data.session.family(selected), ...blockers.values()]);
    for (const member of members) {
      if (root(member) !== selected) continue;
      for (const kind of ["permission", "form"]) {
        const items = api.data.session[kind].list(member);
        if (items === undefined) {
          for (const [key, owner] of blockers) {
            if (owner === member && key.startsWith(`${kind}:`)) next.set(key, owner);
          }
          continue;
        }
        hydrated.add(`${kind}:${member}`);
        for (const item of items) next.set(`${kind}:${item.id}`, member);
      }
    }
    for (const [key, change] of blockerChanges) {
      if (hydrated.has(`${change.kind}:${change.id}`) && next.has(key) === change.present) {
        blockerChanges.delete(key);
      } else if (change.present) {
        next.set(key, change.id);
      } else {
        next.delete(key);
      }
    }
    const changed = (blockers.size > 0) !== (next.size > 0);
    blockers = next;
    return changed;
  }

  function syncSelection() {
    if (disposed) return;
    const id = current();
    if (id !== selected) {
      selected = id;
      generation += 1;
      retryIndex = 0;
      nextSelectionAt = 0;
      blockers.clear();
      blockerChanges.clear();
      if (id) {
        state = api.data.session.status(id) === "running" ? "working" : "idle";
      }
    }
    if (!id) return;
    const blockersChanged = reconcileBlockers();
    if (Date.now() < nextSelectionAt) {
      if (blockersChanged) publish();
      return;
    }
    enqueue(undefined);
    publish();
    const delay = SELECTION_RETRY_DELAYS_MS[retryIndex++];
    nextSelectionAt = delay === undefined ? Number.POSITIVE_INFINITY : Date.now() + delay;
  }

  function receive({ details: event }) {
    if (disposed) return;
    const data = event.data;
    if (data == null) return;
    if (event.type === "session.created") {
      sessions.set(data.sessionID, { id: data.sessionID, parentID: data.parentID });
    }
    if (event.type === "session.deleted") {
      const affected = data.sessionID === selected || [...blockers.values()].includes(data.sessionID);
      sessions.delete(data.sessionID);
      // Deletion is delivered after the cache can remove the session. Use
      // stored ownership rather than looking up the deleted child's ancestry.
      for (const [key, owner] of blockers) if (owner === data.sessionID) blockers.delete(key);
      for (const [key, change] of blockerChanges) {
        if (change.id === data.sessionID) blockerChanges.delete(key);
      }
      syncSelection();
      if (selected && affected) publish();
      return;
    }
    syncSelection();
    const id = event.type === "form.created" ? data.form.sessionID : data.sessionID;
    if (!selected || root(id) !== selected) return;
    switch (event.type) {
      case "permission.asked":
        changeBlocker(id, "permission", data.id, true);
        break;
      case "permission.replied":
        changeBlocker(id, "permission", data.requestID, false);
        break;
      case "form.created":
        changeBlocker(id, "form", data.form.id, true);
        break;
      case "form.replied":
      case "form.cancelled":
        changeBlocker(id, "form", data.id, false);
        break;
      case "session.execution.started":
        if (id !== selected) return;
        state = "working";
        break;
      case "session.execution.succeeded":
      case "session.execution.interrupted":
        if (id !== selected) return;
        state = "idle";
        break;
      case "session.execution.failed":
        if (id !== selected) return;
        state = "blocked";
        break;
      default:
        return;
    }
    publish();
  }

  const unsubscribe = api.data.listen(receive);
  syncSelection();
  const poll = setInterval(syncSelection, ROUTE_POLL_INTERVAL_MS);
  return () => {
    disposed = true;
    generation += 1;
    clearTimeout(retryTimer);
    clearInterval(poll);
    unsubscribe();
    sessions.clear();
    blockers.clear();
    blockerChanges.clear();
  };
}
