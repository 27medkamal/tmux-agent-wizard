# Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

"""Hermes plugin installed by taw to report resumable session identity.

The taw binary is the absolute path baked in at install time. Inside a sandbox (TAW_REPORT_FILE
set) it writes the session to that file instead, and writes nothing when TAW_REPORT_AGENT names
another agent.

installed by: taw integration install {{AGENT}}
managed by taw; reinstalling or updating the integration overwrites this file.
"""

# TAW_INTEGRATION_AGENT={{AGENT}}
# TAW_INTEGRATION_VERSION=2

from __future__ import annotations

import json
import os
import subprocess
import time

_TAW_BIN = {{TAW_BIN}}
_SOURCE = "integration:{{AGENT}}"
_AGENT = "{{AGENT}}"
# Hermes runs headless too; only these platforms have a terminal UI a taw pane can show.
_INTERACTIVE_PLATFORMS = {"cli", "tui", "desktop", "acp"}


def _pane_id() -> str | None:
    return os.environ.get("TMUX_PANE", "").strip() or None


def _write_report(session_id: str, start_source: str) -> None:
    """Inside a sandbox (taw's own): the session goes to TAW_REPORT_FILE in the
    shape of Claude's SessionStart, beside the file and renamed into place."""
    # Another agent's plugin in here writes nothing over the sandbox's own agent's report.
    named = os.environ.get("TAW_REPORT_AGENT")
    if named and named != _AGENT:
        return
    report = os.environ["TAW_REPORT_FILE"]
    staged = f"{report}.{os.getpid()}"
    try:
        with open(staged, "w", encoding="utf-8") as handle:
            json.dump({"hook_event_name": "SessionStart", "session_id": session_id, "source": start_source, "taw_agent": _AGENT}, handle)
        os.replace(staged, report)
    except Exception:
        try:
            os.remove(staged)
        except Exception:
            pass


def _send_session(session_id: str, start_source: str) -> None:
    if os.environ.get("TAW_REPORT_FILE"):
        _write_report(session_id, start_source)
        return
    pane_id = _pane_id()
    if pane_id is None:
        return
    command = [
        _TAW_BIN,
        "pane",
        "report-agent-session",
        pane_id,
        "--source",
        _SOURCE,
        "--agent",
        _AGENT,
        "--seq",
        str(time.time_ns()),
        "--agent-session-id",
        session_id,
        "--session-start-source",
        start_source,
    ]
    try:
        subprocess.run(
            command,
            check=False,
            timeout=5,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        pass


def _report_session(start_source: str, **kwargs) -> None:
    if kwargs.get("platform") not in _INTERACTIVE_PLATFORMS:
        return
    session_id = kwargs.get("session_id")
    if not isinstance(session_id, str) or not session_id:
        return
    _send_session(session_id, start_source)


def _session_started(**kwargs) -> None:
    _report_session("startup", **kwargs)


def _session_reset(**kwargs) -> None:
    _report_session("new", **kwargs)


def _session_observed(**kwargs) -> None:
    if kwargs.get("platform") == "cli":
        _report_session("resume", **kwargs)


def register(ctx):
    ctx.register_hook("on_session_start", _session_started)
    ctx.register_hook("on_session_reset", _session_reset)
    ctx.register_hook("pre_llm_call", _session_observed)
