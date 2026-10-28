#!/usr/bin/env bash
# Renders docs/assets/social-preview.png (1280x640), the image GitHub, X, Reddit
# and chat apps show when the repo or the try page is shared: the headline beside a
# picture of the popup. The picture is the one the tape takes (its Screenshot line, at the
# moment the full tree shows with the cursor on web's blocked agent and its preview showing
# the prompt), so run this after `python3 scripts/demo/make-gif.py`.
#
#   scripts/demo/social-preview.sh [picture.png]   # default dist/popup-frame.png
#
# TAW_CARD_OUT names another file to write, for a trial run.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
FRAME="${1:-$REPO/dist/popup-frame.png}"
OUT="${TAW_CARD_OUT:-$REPO/docs/assets/social-preview.png}"
[ -f "$FRAME" ] || { echo "no $FRAME: record the demo first (python3 scripts/demo/make-gif.py)" >&2; exit 1; }

chrome=""
for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" google-chrome chromium chromium-browser; do
  if command -v "$c" >/dev/null 2>&1 || [ -x "$c" ]; then
    chrome="$c"
    break
  fi
done
[ -n "$chrome" ] || { echo "no Chrome or Chromium to render with" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$HERE/social-preview.html" "$work/index.html"
cp "$FRAME" "$work/popup-frame.png"

rm -f "$OUT"
"$chrome" --headless=new --disable-gpu --no-first-run --use-mock-keychain --password-store=basic \
  --user-data-dir="$work/profile" --hide-scrollbars --window-size=1280,640 \
  --virtual-time-budget=2000 --screenshot="$OUT" "file://$work/index.html" >/dev/null 2>&1 &
browser=$!
# Headless Chrome can stay up after it has written the screenshot, so wait for the
# file instead of for the browser, then close it.
for _ in $(seq 1 150); do
  [ -s "$OUT" ] && break
  sleep 0.2
done
sleep 0.5
# It ignores a plain kill in headless mode; it is a throwaway instance with its
# own temporary profile, so a hard stop loses nothing.
kill -9 "$browser" 2>/dev/null || true
wait "$browser" 2>/dev/null || true
[ -s "$OUT" ] || { echo "the screenshot was not written" >&2; exit 1; }
echo "$OUT"
