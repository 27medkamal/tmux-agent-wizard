// Stands in for `node:child_process` in the OpenCode plugin tests. A run of taw is recorded
// as the request herdr's plugin would have sent over its socket for the same report (the
// method, and the params by their herdr names), so the assertions read the same as herdr's own
// tests do; `args` keeps the argument list itself.
//
// A run is recorded when it starts, because from then on it will happen. Its callback comes
// on the next microtask, or when the test lets go of it (`control.hold`), or with an error and
// nothing recorded (`control.fail`).

export const requests = [];

export const control = {
  hold: false,
  fail: false,
  /** Callbacks of the runs that are being held, in the order they started. */
  held: [],
  /** Resolved by the next run that starts, and by the next run that carries a state. */
  waiters: [],
  stateWaiters: [],
};

function flag(args, name) {
  const at = args.indexOf(name);
  return at === -1 ? undefined : args[at + 1];
}

/** The request herdr would have sent for `args`, which is one run of `taw pane report-*`. */
function requestOf(args, options) {
  const verb = args[1];
  const seq = flag(args, "--seq");
  return {
    args,
    options,
    method: verb === "report-agent" ? "pane.report_agent" : "pane.report_agent_session",
    params: {
      pane_id: args[2],
      source: flag(args, "--source"),
      agent: flag(args, "--agent"),
      agent_session_id: flag(args, "--agent-session-id"),
      state: flag(args, "--state"),
      seq: seq === undefined ? undefined : Number(seq),
      session_start_source: flag(args, "--session-start-source"),
    },
  };
}

export function execFile(bin, args, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  if (bin !== process.env.TAW_BIN_EXPECTED) {
    throw new Error(`the plugin ran ${bin}, not the taw it was installed with`);
  }
  if (args[0] !== "pane" || !["report-agent", "report-agent-session"].includes(args[1])) {
    throw new Error(`the plugin ran an unexpected taw command: ${args.join(" ")}`);
  }
  if (control.fail) {
    queueMicrotask(() => callback(new Error("taw did not run")));
    return { kill() {} };
  }
  const request = requestOf(args, options);
  requests.push(request);
  if (request.params.state !== undefined) control.stateWaiters.shift()?.();
  control.waiters.shift()?.();
  if (control.hold) {
    control.held.push(() => callback(null));
  } else {
    queueMicrotask(() => callback(null));
  }
  return { kill() {} };
}

export default { execFile };
