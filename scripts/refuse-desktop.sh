#!/bin/sh
# What $TAW_NOTIFY_CMD and $TAW_SOUND_CMD name by default in a cargo process of this repository
# (.cargo/config.toml): taw was to show a notification or play a sound, and no test or run said it may.
# It refuses, as a notifier that failed does, and says so. The gate (scripts/desktop-net.sh) names
# recorders of its own, which note the call and refuse it the same way; a test that records toasts sets
# its own.
echo "refused: no recorder for this notification or sound (see .cargo/config.toml), so none is shown" >&2
exit 1
