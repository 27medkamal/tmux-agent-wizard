"""Hermes's plugin inside a sandbox (taw's own), run by
tests/integration_sandbox_plugins.rs with TAW_PLUGIN the rendered plugin."""
import importlib.util
import json
import os
import sys
import tempfile

spec = importlib.util.spec_from_file_location("taw_hermes_plugin", os.environ["TAW_PLUGIN"])
plugin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(plugin)

# Every run of taw the plugin asks for is recorded here and never made.
ran = []
plugin.subprocess.run = lambda command, **_kwargs: ran.append(command)

os.environ.pop("TMUX_PANE", None)
plugin._session_started(platform="cli", session_id="20261002_091201_ab12")
with open(os.environ["TAW_REPORT_FILE"], encoding="utf-8") as handle:
    written = json.load(handle)
expected = {"hook_event_name": "SessionStart", "session_id": "20261002_091201_ab12", "source": "startup", "taw_agent": "hermes"}
if written != expected:
    sys.exit(f"wrote {written!r}, not {expected!r}")
if ran:
    sys.exit(f"ran taw: {ran!r}")

# Each case below writes into a folder of its own inside the one the runner made.
BASE = os.path.dirname(os.environ["TAW_REPORT_FILE"])


def fresh() -> str:
    """A report file in a new folder, set as TAW_REPORT_FILE, with no pane and no agent named."""
    os.environ.pop("TMUX_PANE", None)
    os.environ.pop("TAW_REPORT_AGENT", None)
    ran.clear()
    path = os.path.join(tempfile.mkdtemp(dir=BASE), "session.json")
    os.environ["TAW_REPORT_FILE"] = path
    return path


def report(path: str):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def start(session_id: str, source: str) -> dict:
    return {"hook_event_name": "SessionStart", "session_id": session_id, "source": source, "taw_agent": "hermes"}


def check(what: str, got, want) -> None:
    if got != want:
        sys.exit(f"{what}: got {got!r}, not {want!r}")


# A reset reports new, and a cli turn resume, as they do outside.
path = fresh()
plugin._session_reset(platform="tui", session_id="h-new")
check("a reset", report(path), start("h-new", "new"))
plugin._session_observed(platform="cli", session_id="h-seen")
check("a cli turn", report(path), start("h-seen", "resume"))
check("runs of taw", ran, [])

# The report file is taken first: with TMUX_PANE set as well, taw is still never run.
path = fresh()
os.environ["TMUX_PANE"] = "%1"
plugin._session_started(platform="cli", session_id="h-pane")
check("with TMUX_PANE set", report(path), start("h-pane", "startup"))
check("folder", os.listdir(os.path.dirname(path)), ["session.json"])
check("runs of taw with TMUX_PANE set", ran, [])

# Another agent named in TAW_REPORT_AGENT: nothing is written; its own name writes.
path = fresh()
os.environ["TAW_REPORT_AGENT"] = "claude"
plugin._session_started(platform="cli", session_id="h-other")
check("another agent named", os.listdir(os.path.dirname(path)), [])
os.environ["TAW_REPORT_AGENT"] = "hermes"
plugin._session_started(platform="cli", session_id="h-own")
check("its own name", report(path), start("h-own", "startup"))
check("runs of taw", ran, [])

# A headless platform reports nothing, inside as outside.
path = fresh()
plugin._session_started(platform="telegram", session_id="h-headless")
check("a headless platform", os.listdir(os.path.dirname(path)), [])

# A gone report folder ends quietly and leaves nothing behind.
fresh()
gone = os.path.join(BASE, "gone")
os.environ["TAW_REPORT_FILE"] = os.path.join(gone, "session.json")
plugin._session_started(platform="cli", session_id="h-gone")
check("a gone folder", os.path.exists(gone), False)

# Outside a sandbox it runs as today: with TMUX_PANE alone, taw is told the session.
fresh()
os.environ.pop("TAW_REPORT_FILE")
os.environ["TMUX_PANE"] = "%1"
plugin._session_started(platform="cli", session_id="h-outside")
check("taw runs outside", len(ran), 1)
check("what taw is told", ran[0][1:4] + ran[0][-4:], ["pane", "report-agent-session", "%1", "--agent-session-id", "h-outside", "--session-start-source", "startup"])
